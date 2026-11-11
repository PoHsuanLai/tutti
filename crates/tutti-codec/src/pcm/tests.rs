use super::*;

/// Each integer converter maps 0 to 0, the unit endpoints to the depth's
/// full-scale code (`2^(bits-1) - 1`, symmetric: -full scale is not used), and
/// clamps anything beyond them to the rail.
#[test]
fn integer_converters_clamp_to_the_rail_at_every_depth() {
    let depths: [(&str, fn(f32) -> i32, i32); 2] = [
        ("i16", |s| i32::from(f32_to_i16(s)), 32_767),
        ("i24", f32_to_i24, 8_388_607),
    ];
    for (depth, convert, full) in depths {
        assert_eq!(convert(0.0), 0, "{depth}");
        assert_eq!(convert(1.0), full, "{depth}");
        assert_eq!(convert(-1.0), -full, "{depth}");
        assert_eq!(convert(1.5), full, "{depth}: above the rail");
        assert_eq!(convert(-1.5), -full, "{depth}: below the rail");
    }
}

/// Rounding, not truncation: 0.7 x 32767 = 22936.9 is 22937. The endpoints
/// agree under both, which is how a truncating converter survives unnoticed.
#[test]
fn quantization_rounds_to_nearest() {
    assert_eq!(f32_to_i16(0.7), 22937);
    assert_eq!(f32_to_i16(-0.7), -22937);
    assert_eq!(Pcm::Int16.quantize(0.7), Sample::I16(22937));
}

/// `quantize` dispatches to the converters, at every width, endpoints
/// included, since clamping is where a wrong arm shows up first.
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

/// One LSB is `2^-(bits - 1)`, derived from the width and not read back from
/// the formula: 16 bit is 2^-15, 24 bit is 2^-23.
#[test]
fn the_lsb_is_the_reciprocal_of_full_scale() {
    assert_eq!(IntDepth::Int16.lsb().0, 1.0 / 32768.0);
    assert_eq!(IntDepth::Int24.lsb().0, 1.0 / 8_388_608.0);
}
