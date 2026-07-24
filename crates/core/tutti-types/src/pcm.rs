//! Float→PCM sample quantization.
//!
//! The canonical conversion from a normalized `f32` sample to fixed-point PCM,
//! shared by every codec/sink in the engine (the export encoders, the sampler's
//! live `WavOut`) so a recorded and an exported file quantize a given sample
//! identically instead of each carrying its own copy.
//!
//! Both round to nearest rather than truncating: bare `as iN` truncation biases
//! every sample toward zero (a consistent negative DC error on the negative
//! half), whereas `.round()` is unbiased.

/// Quantize a normalized `f32` (`[-1.0, 1.0]`) to signed 16-bit PCM, clamping
/// out-of-range input.
#[inline]
pub fn f32_to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

/// Quantize a normalized `f32` (`[-1.0, 1.0]`) to signed 24-bit PCM (stored in
/// an `i32`), clamping out-of-range input. 24-bit signed range is
/// `[-8_388_608, 8_388_607]`.
#[inline]
pub fn f32_to_i24(sample: f32) -> i32 {
    (sample.clamp(-1.0, 1.0) * 8_388_607.0).round() as i32
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
