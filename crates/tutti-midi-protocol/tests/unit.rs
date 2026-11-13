//! The float scaling: native UMP integers as unit floats.

use tutti_midi_protocol::{
    signed_f32_to_bend, unit_f32_to_value, unit_f32_to_velocity, value_to_unit_f32,
    velocity_to_unit_f32,
};
use tutti_ump::{BendValue, ControlValue, NoteVelocity};

#[test]
fn unit_f32_inverses_are_exact_enough() {
    for &u in &[0u16, 1, 12345, 40000, u16::MAX] {
        assert_eq!(
            unit_f32_to_velocity(velocity_to_unit_f32(NoteVelocity::new(u))),
            NoteVelocity::new(u)
        );
    }
    // u32 has more bits than f32's mantissa; allow a small relative slack.
    for &u in &[0u32, 1, 0x4000_0000, 0x8000_0000, u32::MAX] {
        let back = unit_f32_to_value(value_to_unit_f32(ControlValue::new(u)));
        let diff = (i64::from(back.get()) - i64::from(u)).unsigned_abs();
        assert!(diff <= 256, "u32 round-trip {u} -> {back:?}");
    }
}

#[test]
fn signed_bend_inverse_centers() {
    assert_eq!(signed_f32_to_bend(0.0), BendValue::new(0x8000_0000));
    assert_eq!(signed_f32_to_bend(-1.0), BendValue::new(0));
    assert_eq!(signed_f32_to_bend(1.0), BendValue::new(u32::MAX));
    // Clamps out-of-range input.
    assert_eq!(signed_f32_to_bend(-2.0), BendValue::new(0));
    assert_eq!(signed_f32_to_bend(2.0), BendValue::new(u32::MAX));
}

/// Half way up the wheel is three quarters of the 32-bit range.
#[test]
fn signed_bend_span_is_half_the_range() {
    assert_eq!(signed_f32_to_bend(0.5), BendValue::new(0xC000_0000));
    assert_eq!(signed_f32_to_bend(-0.5), BendValue::new(0x4000_0000));
}

/// A level of 0, 1 and the 16-bit midpoint are the velocity corners: 0.0 is
/// `0`, 1.0 is `0xFFFF`, and a value past the range clamps rather than wraps.
#[test]
fn a_level_is_a_velocity_between_the_corners() {
    assert_eq!(unit_f32_to_velocity(0.0), NoteVelocity::MIN);
    assert_eq!(unit_f32_to_velocity(1.0), NoteVelocity::MAX);
    assert_eq!(unit_f32_to_velocity(3.0), NoteVelocity::MAX);
    assert_eq!(unit_f32_to_velocity(-1.0), NoteVelocity::MIN);
    assert_eq!(velocity_to_unit_f32(NoteVelocity::MAX), 1.0);
}
