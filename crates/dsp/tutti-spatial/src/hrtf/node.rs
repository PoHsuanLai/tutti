//! [`HrtfBinauralNode`] — the binaural graph node, 2 in / 2 out.
//!
//! Construction needs a measured HRIR sphere, so [`HrtfBinauralNode::new`]
//! takes the dataset bytes.

use tutti_core::ChannelLayout;
use tutti_core::{fold_frame_to_mono, Azimuth, Elevation, Mix, Param, SampleRate, Samples, Tail};
use tutti_graph::{Cx, IntoNode, Io, Node, NodeParts, Prepare, Shape, Status};
use tutti_types::Latency;

use super::panner::{BridgeSample, HrtfBinaural, HrtfBinauralError, LATENCY};
use crate::fork::{fresh_fork_parts, FreshFork};
use crate::SpatialTarget;

/// FFT-convolution binaural panner for headphone 3D audio.
///
/// Position is controlled via lock-free atomics ([`SpatialTarget`]); the audio
/// path reads them once per block. Output — wet *and* dry — lags input by one
/// HRTF frame less one sample, which [`Node::shape`] declares so PDC can
/// compensate it.
pub struct HrtfBinauralNode {
    panner: HrtfBinaural,
    /// The position and blend cells — the same ones the
    /// [`HrtfBinauralControls`] an insert hands back write.
    controls: HrtfBinauralControls,
    sample_rate: SampleRate,
}

/// The live controls of an [`HrtfBinauralNode`]: its position and its
/// HRTF/dry blend, shared with the node (and every clone of these controls).
///
/// What inserting the node hands back ([`IntoNode::Controls`]), and what the
/// node's own setters write before it is inserted. A write lands on the
/// node's next block, lock-free.
///
/// Its own type rather than a [`tutti_graph::ParamSet`]: `UnitParam` has no
/// bearing or height, and a position is a pair (a bearing wraps, a height
/// saturates) that one `f32` address would split.
#[derive(Clone)]
pub struct HrtfBinauralControls {
    target: SpatialTarget,
    blend: Param<Mix>,
}

impl HrtfBinauralControls {
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
        self.blend.store(Mix::new_clamped(blend.into().get()));
    }

    /// The current HRTF/dry [`Mix`], `0..1`.
    pub fn blend(&self) -> Mix {
        self.blend.load()
    }

    /// Stop sharing every cell, keeping the values (see `Param::detach`):
    /// what a fork's copy does, so it renders the placement it was taken at.
    fn detach(&mut self) {
        self.target.detach();
        self.blend.detach();
    }
}

impl Clone for HrtfBinauralNode {
    /// Shares the controls (a clone is the fork's template, which must read
    /// the cells as they are when the fork is taken); the renderer is rebuilt
    /// with cleared streaming state.
    fn clone(&self) -> Self {
        Self {
            panner: self.panner.clone(),
            controls: self.controls.clone(),
            sample_rate: self.sample_rate,
        }
    }
}

impl HrtfBinauralNode {
    /// Build a renderer from HRIR sphere bytes (e.g. an embedded IRCAM `.bin`).
    ///
    /// Fails if the data is unreadable or built for an incompatible rate — the
    /// crate resamples the sphere to `sample_rate` on load. The graph prepares
    /// the node at the device rate before its first block, resampling again if
    /// that differs.
    pub fn new(
        hrir_bytes: &[u8],
        sample_rate: impl Into<SampleRate>,
    ) -> Result<Self, HrtfBinauralError> {
        let sample_rate = sample_rate.into();
        Ok(Self {
            panner: HrtfBinaural::new(hrir_bytes, sample_rate)?,
            controls: HrtfBinauralControls {
                target: SpatialTarget::new(),
                blend: Param::new(Mix::WET),
            },
            sample_rate,
        })
    }

    /// The node's controls: a handle sharing its cells, the same the insert
    /// hands back. Control thread.
    pub fn controls(&self) -> HrtfBinauralControls {
        self.controls.clone()
    }

    /// Bearing (wraps: 0=front, 90=left) and height (saturates: -90..90,
    /// 0=ear level). Lock-free. See [`HrtfBinauralControls::set_position`].
    pub fn set_position(&self, azimuth: impl Into<Azimuth>, elevation: impl Into<Elevation>) {
        self.controls.set_position(azimuth, elevation);
    }

    /// The commanded bearing in [`Azimuth`] degrees — the target, not the
    /// smoothed direction the convolver is currently rendering.
    pub fn azimuth(&self) -> Azimuth {
        self.controls.azimuth()
    }

    /// The commanded height in [`Elevation`] degrees — the target, not the
    /// smoothed direction the convolver is currently rendering.
    pub fn elevation(&self) -> Elevation {
        self.controls.elevation()
    }

    /// Blend between the HRTF-rendered signal and the dry mono center. See
    /// [`HrtfBinauralControls::set_blend`].
    pub fn set_blend(&self, blend: impl Into<Mix>) {
        self.controls.set_blend(blend);
    }

    /// The current HRTF/dry [`Mix`], `0..1`.
    pub fn blend(&self) -> Mix {
        self.controls.blend()
    }

    #[inline]
    fn sync_position(&mut self) {
        let (azimuth, elevation) = self.controls.target.load();
        self.panner.set_position(azimuth, elevation);
    }

    /// Render one input sample pair to a binaural output pair.
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

/// A native node: stereo in, stereo out.
///
/// # Latency and tail
///
/// The frame bridge's [`LATENCY`] (`FRAME_LEN - 1`) is declared as the node's
/// processing latency. The `AudioUnit` this was used to route input 0 straight
/// through — zero latency — while every output sample left a frame late, so
/// PDC never compensated a binaural track and it arrived late against the rest
/// of the mix (design doc 013, D2). The tail is the frame bridge plus the
/// convolution overlap, both fixed sizes: exact in the same way the
/// convolver's is, not an estimate.
///
/// # Reset clears time, not placement
///
/// [`reset`](Node::reset) clears the frame bridge, the convolution tails and
/// the de-zipper ramp. Position and blend are caller-set configuration and
/// survive — see [`VbapPannerNode`](crate::VbapPannerNode)'s `Node` impl for
/// why a reset that re-aims is a silent bug rather than a tidy default.
///
/// The leading `sync_position` is the same fix, and for the same reason: the
/// commanded direction lives in this node's [`SpatialTarget`] and the inner
/// panner's own copy, joined only by that call, which used to run only inside
/// the render. Without it a `set_position` → `reset` seeded the ramp at the
/// panner's stale direction and the first block after the reset rendered from
/// front-centre.
impl Node for HrtfBinauralNode {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::STEREO, ChannelLayout::STEREO)
            .with_latency(Latency::new(Samples(LATENCY)))
            .with_tail(Tail::Finite(Samples(self.panner.ring_out())))
    }

    /// Resamples the sphere to the graph's rate (allocates; control thread).
    fn prepare(&mut self, p: &Prepare) {
        self.sample_rate = p.sample_rate();
        self.panner.set_sample_rate(self.sample_rate);
    }

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        self.sync_position();
        let width = self.controls.blend.load();
        let size = io.frames();
        let (inputs, mut outputs) = io.split();
        let (left, right) = (&inputs.get(0)[..size], &inputs.get(1)[..size]);
        for i in 0..size {
            let (out_left, out_right) = self.render(left[i], right[i], width);
            outputs.get(0)[i] = out_left;
            outputs.get(1)[i] = out_right;
        }
        Status::Modified
    }

    fn reset(&mut self) {
        self.sync_position();
        self.panner.reset_state();
    }
}

impl FreshFork for HrtfBinauralNode {
    /// A clone (a rebuilt renderer) with its position and blend cells
    /// detached at their values now, and reset.
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.controls.detach();
        Node::reset(&mut fork);
        fork
    }
}

/// Inserted with its [`HrtfBinauralControls`], and a fork that shares
/// nothing with it and starts from the placement last set through them.
impl IntoNode for HrtfBinauralNode {
    type Controls = HrtfBinauralControls;

    fn into_parts(self) -> NodeParts<HrtfBinauralControls> {
        let controls = self.controls();
        fresh_fork_parts(self, controls)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_graph::contract::{drive, prepared};

    const RATE: SampleRate = SampleRate(44_100.0);
    const BLOCK: usize = 64;

    /// One block of one frame (what `AudioUnit::tick` was).
    fn tick(node: &mut HrtfBinauralNode, input: [f32; 2]) -> [f32; 2] {
        let out = drive(node, RATE, &[&input[..1], &input[1..]], &[]);
        [out[0][0], out[1][0]]
    }

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
        let node = HrtfBinauralNode::new(&bytes, RATE).expect("synthetic sphere should parse");
        prepared(node, RATE, BLOCK)
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
        for n in 0..(crate::hrtf::panner::FRAME_LEN * 2) {
            let s = ((n as f32) * 0.05).sin();
            let out = tick(&mut node, [s, s]);
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
    /// frame `at`, in 64-frame blocks (the hot path, not one frame at a
    /// time). Returns the (left, right) outputs.
    fn impulse_response(
        node: &mut HrtfBinauralNode,
        at: usize,
        frames: usize,
    ) -> (Vec<f32>, Vec<f32>) {
        let (mut l, mut r) = (Vec::new(), Vec::new());
        let mut done = 0;
        while done < frames {
            let n = BLOCK.min(frames - done);
            let input: Vec<f32> = (0..n)
                .map(|i| if done + i == at { 1.0 } else { 0.0 })
                .collect();
            let out = drive(node, RATE, &[&input, &input], &[]);
            l.extend_from_slice(&out[0]);
            r.extend_from_slice(&out[1]);
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

    /// The declared latency must be the delay the output really has, on both
    /// outputs — design doc 013, D2. The `AudioUnit`'s `route` used to pass
    /// the input straight through (zero), so PDC never compensated a binaural
    /// track.
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
    /// Mutation (run): declaring `Latency::ZERO` in `shape` fails the
    /// declaration assertion (reports 0). Mutation: `LATENCY = FRAME_LEN`
    /// fails every impulse (the doc's figure is one frame too late).
    #[test]
    fn reported_latency_is_the_measured_impulse_delay() {
        use crate::hrtf::panner::{FRAME_LEN, LATENCY};
        let node = make_node();
        assert_eq!(node.shape().latency, Latency::new(Samples(FRAME_LEN - 1)));

        for at in [0, 1, FRAME_LEN - 1, FRAME_LEN, 2 * FRAME_LEN + 37] {
            let mut node = make_node();
            node.set_position(Azimuth(90.0), Elevation::LEVEL);
            Node::reset(&mut node);
            let (l, r) = impulse_response(&mut node, at, at + 3 * FRAME_LEN);
            assert_eq!(onsets(&l), vec![at + LATENCY], "left, impulse at {at}");
            assert_eq!(onsets(&r), vec![at + LATENCY], "right, impulse at {at}");
        }
    }

    /// Through PDC: a binaural track on outputs 0/1 beside a dry path on 2,
    /// in a native graph. The compiler must delay the dry path by the
    /// declared latency, so an impulse leaves all three outputs on one frame;
    /// with the old pass-through `route` nothing was compensated and the
    /// binaural track simply arrived late.
    ///
    /// Mutation (run): declaring `Latency::ZERO` in `shape` → the dry onset
    /// leaves at frame 0, `LATENCY` early → fails.
    #[test]
    fn pdc_compensates_the_other_path_by_the_binaural_latency() {
        use crate::hrtf::panner::LATENCY;
        use tutti_graph::GraphBuilder;
        use tutti_nodes::testing::Through;

        let mut node = make_node();
        node.set_position(Azimuth(90.0), Elevation::LEVEL);
        Node::reset(&mut node);
        let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::from_count(3));
        let (hrtf, _) = g.add_with_controls(node);
        let dry = g.add(Through::mono());
        g.connect_input(0, hrtf, 0);
        g.connect_input(0, hrtf, 1);
        g.connect_input(0, dry, 0);
        g.connect_output(hrtf, 0, 0);
        g.connect_output(hrtf, 1, 1);
        g.connect_output(dry, 0, 2);
        let mut r = g
            .renderer(Prepare::new(RATE, Samples(BLOCK)))
            .expect("builds");
        let mut impulse = vec![0.0f32; 2 * LATENCY];
        impulse[0] = 1.0;
        let out = r.render_input(&[&impulse]);
        for (o, ch) in out.iter().enumerate() {
            assert_eq!(onsets(ch), vec![LATENCY], "output {o}");
        }
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
            Node::reset(&mut node);
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
    /// buffers and the de-zipper ramp, never the caller's placement. A fork is
    /// reset before it renders, and a clone shares these atomics, so a reset
    /// that re-aimed would move the live source too.
    #[test]
    fn reset_keeps_the_authored_placement() {
        let mut node = make_node();
        node.set_position(Azimuth(45.0), Elevation(10.0));
        node.set_blend(Mix(0.4));

        Node::reset(&mut node);

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
        for n in 0..(crate::hrtf::panner::FRAME_LEN * 3) {
            let s = ((n as f32) * 0.05).sin();
            tick(&mut node, [s, s]);
        }

        Node::reset(&mut node);

        // Silence in. With the tail dropped the frames that follow are silent
        // too; a retained tail would ring out through them.
        let mut peak = 0.0f32;
        for _ in 0..(crate::hrtf::panner::FRAME_LEN * 2) {
            let out = tick(&mut node, [0.0, 0.0]);
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
    /// A clone is the fork source's template, taken at insert, so a
    /// snapshotting clone would silently freeze every later export at the
    /// bearing set at insert. (The fork itself detaches; see
    /// `tests/isolate_snapshots.rs`.)
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
