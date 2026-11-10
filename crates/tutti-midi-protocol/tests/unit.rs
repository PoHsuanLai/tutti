//! The float scaling: native UMP integers as unit floats.
//!
//! Moved from `tutti-midi-types`' `convert`, assertions unchanged.
//!
//! Mutation (run once each, restored): `unit_f32_to_u16` truncating instead of
//! rounding fails `unit_f32_inverses_are_exact_enough`; dropping the
//! `0x8000_0000 +` centre in `signed_f32_to_bend_u32` fails
//! `signed_bend_inverse_centers`; doubling its span fails
//! `signed_bend_span_is_half_the_range`. Its two clamps are redundant for the
//! out-of-range assertions (the cast saturates), so removing one alone is not
//! detectable here.

use tutti_midi_protocol::{
    signed_f32_to_bend_u32, u16_to_unit_f32, u32_to_unit_f32, unit_f32_to_u16, unit_f32_to_u32,
};

#[test]
fn unit_f32_inverses_are_exact_enough() {
    for &u in &[0u16, 1, 12345, 40000, u16::MAX] {
        assert_eq!(unit_f32_to_u16(u16_to_unit_f32(u)), u);
    }
    // u32 has more bits than f32's mantissa; allow a small relative slack.
    for &u in &[0u32, 1, 0x4000_0000, 0x8000_0000, u32::MAX] {
        let back = unit_f32_to_u32(u32_to_unit_f32(u));
        let diff = (back as i64 - u as i64).unsigned_abs();
        assert!(diff <= 256, "u32 round-trip {u} -> {back}");
    }
}

#[test]
fn signed_bend_inverse_centers() {
    assert_eq!(signed_f32_to_bend_u32(0.0), 0x8000_0000);
    assert_eq!(signed_f32_to_bend_u32(-1.0), 0);
    assert_eq!(signed_f32_to_bend_u32(1.0), u32::MAX);
    // Clamps out-of-range input.
    assert_eq!(signed_f32_to_bend_u32(-2.0), 0);
    assert_eq!(signed_f32_to_bend_u32(2.0), u32::MAX);
}

/// Half way up the wheel is three quarters of the 32-bit range.
#[test]
fn signed_bend_span_is_half_the_range() {
    assert_eq!(signed_f32_to_bend_u32(0.5), 0xC000_0000);
    assert_eq!(signed_f32_to_bend_u32(-0.5), 0x4000_0000);
}
