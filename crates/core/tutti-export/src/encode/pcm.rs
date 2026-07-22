//! Shared float→PCM sample conversion.
//!
//! The WAV and AIFF encoders both quantize normalized `f32` samples to fixed
//! point the same way; this is the one definition they share instead of each
//! carrying an identical copy.

/// Quantize a normalized `f32` (`[-1.0, 1.0]`) to signed 16-bit PCM, clamping
/// out-of-range input.
#[inline]
pub(crate) fn f32_to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * 32767.0) as i16
}

/// Quantize a normalized `f32` (`[-1.0, 1.0]`) to signed 24-bit PCM (stored in
/// an `i32`), clamping out-of-range input.
#[inline]
pub(crate) fn f32_to_i24(sample: f32) -> i32 {
    (sample.clamp(-1.0, 1.0) * 8388607.0) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f32_to_i16_clamps() {
        assert_eq!(f32_to_i16(0.0), 0);
        assert_eq!(f32_to_i16(1.0), 32767);
        assert_eq!(f32_to_i16(-1.0), -32767);
        assert_eq!(f32_to_i16(1.5), 32767);
        assert_eq!(f32_to_i16(-1.5), -32767);
    }

    #[test]
    fn f32_to_i24_clamps() {
        assert_eq!(f32_to_i24(0.0), 0);
        assert_eq!(f32_to_i24(1.0), 8388607);
        assert_eq!(f32_to_i24(-1.0), -8388607);
        assert_eq!(f32_to_i24(1.5), 8388607);
        assert_eq!(f32_to_i24(-1.5), -8388607);
    }
}
