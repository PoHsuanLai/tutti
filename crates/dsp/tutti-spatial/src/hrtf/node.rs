//! [`HrtfBinauralNode`] — the binaural `AudioUnit`, 2 in / 2 out.
//!
//! Construction needs a measured HRIR sphere, so [`HrtfBinauralNode::new`]
//! takes the dataset bytes.

use tutti_core::ChannelLayout;
use tutti_core::{
    fold_frame_to_mono, AudioUnit, Azimuth, BufferMut, BufferRef, Elevation, Mix, Param,
    SampleRate, Samples, SignalFrame, Tail,
};

use super::panner::{BridgeSample, HrtfBinaural, HrtfBinauralError, LATENCY};
use crate::SpatialTarget;

/// FFT-convolution binaural panner for headphone 3D audio.
///
/// Position is controlled via lock-free atomics ([`SpatialTarget`]); the audio
/// path reads them once per block. Output — wet *and* dry — lags input by one
/// HRTF frame less one sample, which `route` reports so PDC can compensate it.
pub struct HrtfBinauralNode {
    panner: HrtfBinaural,
    target: SpatialTarget,
    width: Param<Mix>,
    sample_rate: SampleRate,
}

impl Clone for HrtfBinauralNode {
    fn clone(&self) -> Self {
        Self {
            panner: self.panner.clone(),
            target: self.target.clone(),
            width: self.width.handle(),
            sample_rate: self.sample_rate,
        }
    }
}

impl HrtfBinauralNode {
    /// Build a renderer from HRIR sphere bytes (e.g. an embedded IRCAM `.bin`).
    ///
    /// Fails if the data is unreadable or built for an incompatible rate — the
    /// crate resamples the sphere to `sample_rate` on load.
    pub fn new(
        hrir_bytes: &[u8],
        sample_rate: impl Into<SampleRate>,
    ) -> Result<Self, HrtfBinauralError> {
        let sample_rate = sample_rate.into();
        Ok(Self {
            panner: HrtfBinaural::new(hrir_bytes, sample_rate)?,
            target: SpatialTarget::new(),
            width: Param::new(Mix::WET),
            sample_rate,
        })
    }

    /// Bearing (wraps: 0=front, 90=left) and height (saturates: -90..90,
    /// 0=ear level). Lock-free.
    pub fn set_position(&self, azimuth: impl Into<Azimuth>, elevation: impl Into<Elevation>) {
        self.target.store(azimuth, elevation);
    }

    /// The commanded bearing in [`Azimuth`] degrees — the target, not the
    /// smoothed direction the convolver is currently rendering.
    pub fn azimuth(&self) -> Azimuth {
        self.target.azimuth.load()
    }

    /// The commanded height in [`Elevation`] degrees — the target, not the
    /// smoothed direction the convolver is currently rendering.
    pub fn elevation(&self) -> Elevation {
        self.target.elevation.load()
    }

    /// Blend between the HRTF-rendered signal and the dry mono center:
    /// 1.0 = full HRTF, 0.0 = passthrough.
    ///
    /// Named `blend`, not `width`: VBAP's `set_width` is a virtual-source
    /// spread in `StereoWidth`, while this is a `Mix`. Same word, different
    /// unit and different meaning.
    pub fn set_blend(&self, blend: impl Into<Mix>) {
        self.width.store(Mix::new_clamped(blend.into().get()));
    }

    /// The current HRTF/dry [`Mix`], `0..1`.
    pub fn blend(&self) -> Mix {
        self.width.load()
    }

    #[inline]
    fn sync_position(&mut self) {
        let (azimuth, elevation) = self.target.load();
        self.panner.set_position(azimuth, elevation);
    }

    /// Render one interleaved input sample pair to a binaural output pair.
    #[inline]
    fn render(&mut self, left: f32, right: f32, width: Mix) -> (f32, f32) {
        // The engine's one fold, not a local `* 0.5`. Identical at width 2, but
        // HRTF rendering genuinely needs a single mono sample, so the coefficient
        // belongs to `downmix.rs` rather than to this node.
        let mono = fold_frame_to_mono(&[left, right]);
        // The dry mono comes back out of the frame bridge beside the wet pair,
        // equally late. Blending against `mono` itself led the wet signal by
        // the whole frame, so at any `blend < 1` the dry half arrived early.
        let BridgeSample {
            dry,
            wet: (wet_l, wet_r),
        } = self.panner.process_sample(mono);
        // width blends the HRTF-rendered signal against the dry mono center.
        (width.blend(dry, wet_l), width.blend(dry, wet_r))
    }
}

impl AudioUnit for HrtfBinauralNode {
    fn inputs(&self) -> usize {
        2
    }

    fn outputs(&self) -> usize {
        2
    }

    /// Clears the frame bridge, the convolution tails and the de-zipper ramp.
    /// Position and blend are caller-set configuration and survive — see
    /// [`VbapPannerNode::reset`](crate::VbapPannerNode) for why a reset that
    /// re-aims is a silent bug rather than a tidy default.
    ///
    /// The leading [`sync_position`](Self::sync_position) is the same fix, and
    /// for the same reason: the commanded direction lives in this node's
    /// [`SpatialTarget`] and the inner panner's own copy, joined only by that
    /// call, which used to run only inside `tick`/`process`. Without it a
    /// `set_position` → `reset` seeded the ramp at the panner's stale direction
    /// and the first block after the reset rendered from front-centre.
    fn reset(&mut self) {
        self.sync_position();
        self.panner.reset_state();
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.sample_rate = sample_rate;
        self.panner.set_sample_rate(sample_rate);
    }

    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        self.sync_position();
        let width = self.width.load();

        let left = input.first().copied().unwrap_or(0.0);
        let right = input.get(1).copied().unwrap_or(left);

        let (out_left, out_right) = self.render(left, right, width);
        if output.len() >= 2 {
            output[0] = out_left;
            output[1] = out_right;
        }
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        self.sync_position();
        let width = self.width.load();

        // Hoisted once per block: is a second input channel present?
        let has_stereo_in = ChannelLayout::from(input.channels()).is_multi();

        for i in 0..size {
            let left = input.at_f32(0, i);
            let right = if has_stereo_in {
                input.at_f32(1, i)
            } else {
                left
            };
            let (out_left, out_right) = self.render(left, right, width);
            output.set_f32(0, i, out_left);
            output.set_f32(1, i, out_right);
        }
    }

    fn get_id(&self) -> u64 {
        crate::node_id::HRTF_BINAURAL_ID
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    /// The frame bridge's [`LATENCY`] on both outputs.
    ///
    /// This used to pass input 0 straight through — zero latency — while every
    /// output sample left a frame late, so PDC never compensated a binaural
    /// track and it arrived late against the rest of the mix (design doc 013,
    /// D2). Both outputs are a fold of *both* inputs, hence the combine; and a
    /// moving HRIR has no fixed frequency response, hence nonlinear.
    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut output = SignalFrame::new(2);
        let rendered = input.at(0).combine_nonlinear(input.at(1), LATENCY as f64);
        output.set(0, rendered);
        output.set(1, rendered);
        output
    }

    /// The frame bridge plus the convolution overlap, both fixed sizes.
    ///
    /// HRTF rendering convolves against a measured HRIR, so this is exact in the
    /// same way the convolver's is — not an estimate.
    fn tail(&mut self) -> Tail {
        Tail::Finite(Samples(self.panner.ring_out()))
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize a minimal but format-valid HRIR sphere: a tetrahedron of 4
    /// vertices with unit-impulse HRIRs (left = identity delta, right scaled).
    /// Enough for the crate to parse, triangulate, and convolve — no dataset
    /// file needed to exercise the bridge.
    fn synthetic_hrir_sphere(sample_rate: u32, ir_len: usize) -> Vec<u8> {
        fn push_u32(b: &mut Vec<u8>, v: u32) {
            b.extend_from_slice(&v.to_le_bytes());
        }
        fn push_f32(b: &mut Vec<u8>, v: f32) {
            b.extend_from_slice(&v.to_le_bytes());
        }

        let verts: [[f32; 3]; 4] = [
            [1.0, 1.0, 1.0],
            [-1.0, -1.0, 1.0],
            [-1.0, 1.0, -1.0],
            [1.0, -1.0, -1.0],
        ];
        let faces: [[u32; 3]; 4] = [[0, 1, 2], [0, 3, 1], [0, 2, 3], [1, 3, 2]];

        let mut b = Vec::new();
        b.extend_from_slice(b"HRIR");
        push_u32(&mut b, sample_rate);
        push_u32(&mut b, ir_len as u32);
        push_u32(&mut b, verts.len() as u32);
        push_u32(&mut b, (faces.len() * 3) as u32);
        for f in faces {
            for idx in f {
                push_u32(&mut b, idx);
            }
        }
        for (vi, v) in verts.iter().enumerate() {
            push_f32(&mut b, v[0]);
            push_f32(&mut b, v[1]);
            push_f32(&mut b, v[2]);
            // Left HRIR: unit delta at t=0.
            for i in 0..ir_len {
                push_f32(&mut b, if i == 0 { 1.0 } else { 0.0 });
            }
            // Right HRIR: delta scaled per-vertex so L and R differ.
            for i in 0..ir_len {
                push_f32(&mut b, if i == 0 { 0.5 + 0.1 * vi as f32 } else { 0.0 });
            }
        }
        b
    }

    fn make_node() -> HrtfBinauralNode {
        let bytes = synthetic_hrir_sphere(44_100, 64);
        HrtfBinauralNode::new(&bytes, 44_100.0).expect("synthetic sphere should parse")
    }

    #[test]
    fn rejects_garbage_bytes() {
        let err = HrtfBinauralNode::new(&[0u8; 16], 44_100.0);
        assert!(err.is_err(), "non-HRIR bytes must fail to construct");
    }

    #[test]
    fn produces_output_after_one_frame_of_latency() {
        let mut node = make_node();
        node.set_position(90.0, 0.0);

        // Drive a steady tone through more than one HRTF frame and confirm the
        // renderer eventually emits non-silence (output lags by one frame).
        let mut produced_nonzero = false;
        let mut out = [0.0f32; 2];
        for n in 0..(crate::hrtf::panner::FRAME_LEN * 2) {
            let s = ((n as f32) * 0.05).sin();
            node.tick(&[s, s], &mut out);
            if out[0].abs() > 1e-6 || out[1].abs() > 1e-6 {
                produced_nonzero = true;
            }
        }
        assert!(
            produced_nonzero,
            "HRTF node should emit audio after warm-up"
        );
    }

    /// Run `node` over `frames` frames with a unit impulse on both inputs at
    /// frame `at`, through `process` in 64-frame blocks (the hot path, not
    /// `tick`). Returns the (left, right) outputs.
    fn impulse_response(
        node: &mut HrtfBinauralNode,
        at: usize,
        frames: usize,
    ) -> (Vec<f32>, Vec<f32>) {
        use tutti_core::BufferVec;
        const BLOCK: usize = 64;
        let mut input = BufferVec::new(2);
        let mut output = BufferVec::new(2);
        let (mut l, mut r) = (Vec::new(), Vec::new());
        let mut done = 0;
        while done < frames {
            let n = BLOCK.min(frames - done);
            {
                let mut b = input.buffer_mut();
                for c in 0..2 {
                    for i in 0..BLOCK {
                        b.set_f32(c, i, if done + i == at { 1.0 } else { 0.0 });
                    }
                }
            }
            node.process(n, &input.buffer_ref(), &mut output.buffer_mut());
            let o = output.buffer_ref();
            l.extend((0..n).map(|i| o.at_f32(0, i)));
            r.extend((0..n).map(|i| o.at_f32(1, i)));
            done += n;
        }
        (l, r)
    }

    fn onsets(ch: &[f32]) -> Vec<usize> {
        ch.iter()
            .enumerate()
            .filter(|(_, s)| s.abs() > 1e-4)
            .map(|(i, _)| i)
            .collect()
    }

    /// `route` must report the delay the output really has, on both outputs —
    /// design doc 013, D2. It used to pass the input straight through (zero),
    /// so PDC never compensated a binaural track.
    ///
    /// The figure is **measured**, not read off the doc comment: the module doc
    /// said "one HRTF frame" (512), and an impulse says 511 — the bridge renders
    /// on the push that completes a frame and drains that frame's first sample
    /// in the same call. The synthetic sphere's HRIRs are deltas at t = 0, so
    /// every frame of delay seen here is the bridge's.
    ///
    /// Impulses land at several offsets within a frame (first, second, last,
    /// and across a frame boundary), because a bridge whose delay depended on
    /// the in-frame phase would have no single latency to report.
    ///
    /// Mutation: restoring the pass-through `route` fails the `latency()`
    /// assertion (reports 0). Mutation: `LATENCY = FRAME_LEN` fails every
    /// impulse (the doc's figure is one frame too late).
    #[test]
    fn reported_latency_is_the_measured_impulse_delay() {
        use crate::hrtf::panner::{FRAME_LEN, LATENCY};
        let mut node = make_node();
        assert_eq!(node.latency(), Some((FRAME_LEN - 1) as f64));
        let mut input = SignalFrame::new(2);
        input.set(0, tutti_core::Signal::Latency(0.0));
        input.set(1, tutti_core::Signal::Latency(0.0));
        let routed = node.route(&input, 1.0);
        for o in 0..2 {
            assert!(
                matches!(routed.at(o), tutti_core::Signal::Latency(l) if l == LATENCY as f64),
                "output {o} does not report Latency({LATENCY})"
            );
        }

        for at in [0, 1, FRAME_LEN - 1, FRAME_LEN, 2 * FRAME_LEN + 37] {
            let mut node = make_node();
            node.set_position(Azimuth(90.0), Elevation::LEVEL);
            node.reset();
            let (l, r) = impulse_response(&mut node, at, at + 3 * FRAME_LEN);
            assert_eq!(onsets(&l), vec![at + LATENCY], "left, impulse at {at}");
            assert_eq!(onsets(&r), vec![at + LATENCY], "right, impulse at {at}");
        }
    }

    /// Through PDC: a binaural track on outputs 0/1 beside a dry path on 2. The
    /// dry path must now pre-roll by the bridge's latency; with the old
    /// pass-through `route` the plan was empty and the binaural track simply
    /// arrived late.
    ///
    /// Mutation: restoring the pass-through `route` fails (an empty plan).
    #[test]
    fn pdc_compensates_the_other_path_by_the_binaural_latency() {
        use crate::hrtf::panner::LATENCY;
        use tutti_core::dsp::{pass, Net, Source};
        use tutti_core::latency;

        let mut net = Net::new(1, 3);
        let hrtf = net.add(make_node());
        let dry = net.add(pass());
        net.set_source(hrtf, 0, Source::Global(0));
        net.set_source(hrtf, 1, Source::Global(0));
        net.set_source(dry, 0, Source::Global(0));
        net.set_output_source(0, Source::Local(hrtf, 0));
        net.set_output_source(1, Source::Local(hrtf, 1));
        net.set_output_source(2, Source::Local(dry, 0));

        let plan = latency::plan(&net);
        assert_eq!(plan.total(), Samples(LATENCY));
        assert_eq!(plan.channels(), &[Samples(0), Samples(0), Samples(LATENCY)]);
    }

    /// The dry half of the blend leaves with the wet half, so the reported
    /// latency is true of the whole output at any blend — the D3 shape, which
    /// this node had too: it blended the undelayed `mono` against a wet signal
    /// a frame late.
    ///
    /// Mutation: blending against `mono` instead of the bridge's `dry` fails
    /// blend 0.0 and 0.5 (an onset at the impulse itself). Mutation: dropping
    /// the swap in `FrameBridge::rewind` fails them too (the dry frame is
    /// never refreshed, so the dry half never arrives).
    #[test]
    fn dry_and_wet_leave_together_at_every_blend() {
        use crate::hrtf::panner::{FRAME_LEN, LATENCY};
        for blend in [0.0_f32, 0.5, 1.0] {
            let mut node = make_node();
            node.set_position(Azimuth(90.0), Elevation::LEVEL);
            node.set_blend(Mix(blend));
            node.reset();
            let at = 100;
            let (l, r) = impulse_response(&mut node, at, at + 2 * FRAME_LEN);
            assert_eq!(onsets(&l), vec![at + LATENCY], "left, blend {blend}");
            assert_eq!(onsets(&r), vec![at + LATENCY], "right, blend {blend}");
            if blend == 0.0 {
                // Fully dry is the mono fold, delayed and otherwise untouched.
                assert!((l[at + LATENCY] - 1.0).abs() < 1e-6);
                assert!((r[at + LATENCY] - 1.0).abs() < 1e-6);
            }
        }
    }

    /// Same contract as the VBAP panner's: `reset` clears the streaming
    /// buffers and the de-zipper ramp, never the caller's placement. The
    /// exporter resets a cloned net before rendering, and `Clone` shares these
    /// atomics, so a reset that re-aimed would move the live source too.
    #[test]
    fn reset_keeps_the_authored_placement() {
        let mut node = make_node();
        node.set_position(Azimuth(45.0), Elevation(10.0));
        node.set_blend(Mix(0.4));

        node.reset();

        assert_eq!(node.azimuth(), Azimuth(45.0));
        assert_eq!(node.elevation(), Elevation(10.0));
        assert!(
            (node.blend().get() - 0.4).abs() < 0.001,
            "reset changed the blend to {}",
            node.blend().get()
        );
    }

    /// The other half of the contract: the streaming state *is* dropped, so a
    /// reset between takes does not bleed the previous take's convolution tail
    /// into the next one — which is the whole reason a host calls `reset`.
    #[test]
    fn reset_drops_the_convolution_tail() {
        let mut node = make_node();
        node.set_position(Azimuth(90.0), Elevation::LEVEL);

        // Fill the bridge and the overlap tails with a loud take.
        let mut out = [0.0f32; 2];
        for n in 0..(crate::hrtf::panner::FRAME_LEN * 3) {
            let s = ((n as f32) * 0.05).sin();
            node.tick(&[s, s], &mut out);
        }

        node.reset();

        // Silence in. With the tail dropped the frames that follow are silent
        // too; a retained tail would ring out through them.
        let mut peak = 0.0f32;
        for _ in 0..(crate::hrtf::panner::FRAME_LEN * 2) {
            node.tick(&[0.0, 0.0], &mut out);
            peak = peak.max(out[0].abs()).max(out[1].abs());
        }
        assert!(
            peak < 1e-6,
            "reset left {peak} of the previous take's tail in the buffers"
        );
    }

    /// `Clone` **shares** the position atomics rather than snapshotting them —
    /// parity with [`VbapPannerNode`](crate::vbap::VbapPannerNode), and the
    /// reason `reset` must never write them (see `reset_keeps_the_authored_placement`).
    /// The offline exporter clones the live net, so a snapshotting clone would
    /// silently freeze a render at whatever bearing was set at clone time.
    #[test]
    fn clone_shares_the_position_atomics() {
        let node = make_node();
        let c = node.clone();

        // Set through the original, observe through the clone.
        node.set_position(45.0, 10.0);
        assert_eq!(c.azimuth(), Azimuth(45.0));
        assert_eq!(c.elevation(), Elevation(10.0));

        // And back the other way.
        c.set_position(-60.0, -20.0);
        assert_eq!(node.azimuth(), Azimuth(-60.0));
        assert_eq!(node.elevation(), Elevation(-20.0));
    }
}
