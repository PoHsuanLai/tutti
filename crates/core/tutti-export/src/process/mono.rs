//! Stereo → mono downmix.
//!
//! Encoders that emit a mono file fold each `[f32; 2]` frame with this; every
//! stage upstream stays stereo, so the fold happens once, at the encoder edge.

/// Average a stereo frame to a single mono sample.
#[inline]
pub(crate) fn fold_frame([l, r]: [f32; 2]) -> f32 {
    (l + r) * 0.5
}
