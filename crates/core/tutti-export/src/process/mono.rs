//! Stereo → mono downmix.

/// Convert stereo to mono by averaging channels.
pub(crate) fn stereo_to_mono(left: &[f32], right: &[f32]) -> Vec<f32> {
    left.iter().zip(right).map(|(l, r)| (l + r) * 0.5).collect()
}
