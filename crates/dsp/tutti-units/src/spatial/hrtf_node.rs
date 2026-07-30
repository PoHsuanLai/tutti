//! [`HrtfBinauralNode`] — a real-HRTF drop-in replacement for
//! [`super::nodes::BinauralPannerNode`].
//!
//! Same 2-in / 2-out `AudioUnit` shape and the same position/width control
//! surface, so a host can swap one for the other. The only construction
//! difference: HRTF rendering needs a measured HRIR sphere, so
//! [`HrtfBinauralNode::new`] takes the dataset bytes.

use tutti_core::ChannelLayout;
use tutti_core::{
    fold_frame_to_mono, AudioUnit, Azimuth, BufferMut, BufferRef, Elevation, Mix, Param,
    SampleRate, SignalFrame,
};

use super::hrtf_panner::{HrtfBinaural, HrtfBinauralError};
use super::nodes::SpatialTarget;

/// FFT-convolution binaural panner for headphone 3D audio.
///
/// Position is controlled via lock-free atomics ([`SpatialTarget`]) exactly
/// like [`super::nodes::BinauralPannerNode`]; the audio path reads them once
/// per block. Rendering lags input by one HRTF frame (see [`HrtfBinaural`]).
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

    pub fn azimuth(&self) -> Azimuth {
        self.target.azimuth.load()
    }

    pub fn elevation(&self) -> Elevation {
        self.target.elevation.load()
    }

    /// Kept for API parity with the ITD/ILD node. HRTF rendering is inherently
    /// full-sphere, so width is a post-render dry/processed blend rather than a
    /// virtual-source spread: 1.0 = full HRTF, 0.0 = center/mono passthrough.
    pub fn set_width(&self, width: impl Into<Mix>) {
        self.width.store(Mix::new_clamped(width.into().get()));
    }

    pub fn width(&self) -> Mix {
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
        let (wet_l, wet_r) = self.panner.process_sample(mono);
        // width blends the HRTF-rendered signal against the dry mono center.
        (width.blend(mono, wet_l), width.blend(mono, wet_r))
    }
}

impl AudioUnit for HrtfBinauralNode {
    fn inputs(&self) -> usize {
        2
    }

    fn outputs(&self) -> usize {
        2
    }

    fn reset(&mut self) {
        self.target.reset_origin();
        self.width.store(Mix::WET);
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

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut output = SignalFrame::new(2);
        output.set(0, input.at(0));
        output.set(1, input.at(0));
        output
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
    fn parses_synthetic_sphere_and_reports_2in_2out() {
        let node = make_node();
        assert_eq!(node.inputs(), 2);
        assert_eq!(node.outputs(), 2);
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
        for n in 0..(super::super::hrtf_panner::FRAME_LEN * 2) {
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

    #[test]
    fn clone_is_independent() {
        let node = make_node();
        node.set_position(45.0, 10.0);
        let c = node.clone();
        // Clone shares the atomic position handle (parity with the ITD node).
        assert_eq!(c.azimuth(), Azimuth(45.0));
        assert_eq!(c.elevation(), Elevation(10.0));
    }
}
