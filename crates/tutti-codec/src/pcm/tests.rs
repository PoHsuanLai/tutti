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

/// Rounding, not truncation: 0.7 x 32767 = 22936.9 is 22937. The endpoints
/// agree under both, which is how a truncating converter survives unnoticed.
///
/// Mutation: replacing `.round()` with `.trunc()` in `f32_to_i16` gives 22936
/// and fails the first assertion.
#[test]
fn quantization_rounds_to_nearest() {
    assert_eq!(f32_to_i16(0.7), 22937);
    assert_eq!(f32_to_i16(-0.7), -22937);
    assert_eq!(Pcm::Int16.quantize(0.7), Sample::I16(22937));
}

/// `quantize` dispatches to the converters, at every width, endpoints
/// included, since clamping is where a wrong arm shows up first.
///
/// Mutation: swapping the `Int16` and `Int24` arms of `Pcm::quantize` fails
/// every row.
#[test]
fn quantize_agrees_with_the_converters_at_every_width() {
    for &s in &[0.0f32, 0.5, -0.5, 1.0, -1.0, 1.5, -1.5] {
        assert_eq!(Pcm::Int16.quantize(s), Sample::I16(f32_to_i16(s)), "{s}");
        assert_eq!(Pcm::Int24.quantize(s), Sample::I24(f32_to_i24(s)), "{s}");
        assert_eq!(Pcm::Float32.quantize(s), Sample::F32(s), "{s}");
        assert_eq!(IntDepth::Int16.quantize(s), i32::from(f32_to_i16(s)), "{s}");
        assert_eq!(IntDepth::Int24.quantize(s), f32_to_i24(s), "{s}");
    }
}

/// Each width produces its own variant, so a writer's `match` cannot write
/// 16-bit samples into a 24-bit file. 0.5 is exact at every width, so any
/// difference is the width's, not rounding's.
#[test]
fn each_width_is_distinguishable_at_the_call_site() {
    assert_eq!(Pcm::Int16.quantize(0.5), Sample::I16(16384));
    assert_eq!(Pcm::Int24.quantize(0.5), Sample::I24(4194304));
    assert_eq!(Pcm::Float32.quantize(0.5), Sample::F32(0.5));
    assert_eq!(
        [Pcm::Int16.bits(), Pcm::Int24.bits(), Pcm::Float32.bits()],
        [16, 24, 32]
    );
    assert_eq!(Pcm::Int16.int_depth(), Some(IntDepth::Int16));
    assert_eq!(Pcm::Int24.int_depth(), Some(IntDepth::Int24));
    assert_eq!(
        Pcm::Float32.int_depth(),
        None,
        "float is the one width that does not quantize"
    );
    assert_eq!(Pcm::from(IntDepth::Int16), Pcm::Int16);
    assert_eq!(Pcm::from(IntDepth::Int24).bits(), IntDepth::Int24.bits());
}
