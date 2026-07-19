//! Whole-signal mastering pipeline.
//!
//! Carries two stereo buffers through resample → normalize → dither → mono
//! downmix. Each stage is a thin method that delegates to the matching leaf
//! module; the struct itself is pure orchestration over its children.

use crate::options::{BitDepth, Dither, Normalize};
use crate::process::{
    analyze_loudness, apply_dither, normalize_loudness, normalize_peak, resample_stereo,
    stereo_to_mono, DitherState, ResampleQuality,
};
use crate::Result;

/// Accumulating buffer for whole-signal processing. Each method mutates in
/// place and returns `self`-result so a caller can chain stages.
pub(crate) struct Chain {
    left: Vec<f32>,
    right: Vec<f32>,
    sample_rate: u32,
}

impl Chain {
    pub fn new(left: &[f32], right: &[f32], sample_rate: u32) -> Self {
        Self {
            left: left.to_vec(),
            right: right.to_vec(),
            sample_rate,
        }
    }

    /// Convert to `target` sample rate. No-op when `target == self.sample_rate`.
    pub fn resample_to(&mut self, target: u32, quality: ResampleQuality) -> Result<()> {
        if target == self.sample_rate {
            return Ok(());
        }
        let (l, r) = resample_stereo(&self.left, &self.right, self.sample_rate, target, quality)?;
        self.left = l;
        self.right = r;
        self.sample_rate = target;
        Ok(())
    }

    /// Apply peak or EBU R128 loudness normalization. `Off` is a no-op.
    pub fn normalize(&mut self, mode: Normalize) {
        match mode {
            Normalize::Off => {}
            Normalize::Peak { target_db } => {
                normalize_peak(&mut self.left, &mut self.right, target_db);
            }
            Normalize::Loudness {
                target_lufs,
                true_peak_dbtp,
            } => {
                let current = analyze_loudness(&self.left, &self.right, self.sample_rate);
                normalize_loudness(
                    &mut self.left,
                    &mut self.right,
                    current.lufs,
                    target_lufs,
                    true_peak_dbtp,
                );
            }
        }
    }

    /// Apply dithering for target bit depth. `Dither::Off` is a no-op.
    pub fn dither(&mut self, dither: Dither, bit_depth: BitDepth) {
        if matches!(dither, Dither::Off) {
            return;
        }
        let mut state = DitherState::new(dither);
        apply_dither(
            &mut self.left,
            &mut self.right,
            bit_depth.bits(),
            &mut state,
        );
    }

    pub fn into_stereo(self) -> (Vec<f32>, Vec<f32>) {
        (self.left, self.right)
    }

    pub fn into_mono(self) -> Vec<f32> {
        stereo_to_mono(&self.left, &self.right)
    }
}
