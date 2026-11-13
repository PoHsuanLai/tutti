//! A native UMP integer as the unit float a DSP node wants, and back.
//!
//! These map a native UMP integer to the `[0.0, 1.0]` / `[-1.0, 1.0]` range a
//! DSP node wants. They are a lossy convenience for the audio edge, not part of
//! the MIDI 1.0 to 2.0 resolution path (`widen` and the `narrow_*` family):
//! `value_to_unit_f32` loses low bits through f32's 24-bit mantissa. Consumers
//! call these at their own boundary, not through a shared decode layer.
//!
//! **Where the types stop** (CLAUDE.md, "Unit types"): these return a bare
//! `f32`, and the reason is that the number has no meaning of its own. A
//! normalized `[0, 1]` controller value is a velocity, a pressure, a
//! controller position or a plugin's normalized parameter, depending on who
//! reads it, and a bend in `[-1, 1]` is a position on a range the receiver
//! owns (`PitchBendSensitivity` is that range). No `tutti_types` unit covers
//! these ranges without lending a meaning that is not the packet's (`Mix` is a
//! position among a node's own settings, `Depth` a modulation amount,
//! `Amplitude` a gain), so each caller wraps the value in the unit it means at
//! its own seam. The inverses take the same bare `f32`, as the plugin formats
//! carry it, and clamp.

use tutti_ump::{BendValue, ControlValue, NoteVelocity};

/// 7-bit value (0-127) → `[0.0, 1.0]`.
#[inline]
pub fn u7_to_unit_f32(v: u8) -> f32 {
    f32::from(v) / 127.0
}

/// A MIDI 2.0 velocity → `[0.0, 1.0]`.
#[inline]
pub fn velocity_to_unit_f32(v: NoteVelocity) -> f32 {
    f32::from(v.get()) / f32::from(u16::MAX)
}

/// A MIDI 2.0 controller value → `[0.0, 1.0]`.
#[inline]
pub fn value_to_unit_f32(v: ControlValue) -> f32 {
    (v.get() as f64 / u32::MAX as f64) as f32
}

/// 14-bit pitch bend (center 8192) → `[-1.0, 1.0]`.
#[inline]
pub fn bend_u14_to_signed_f32(v: u16) -> f32 {
    (f32::from(v) - 8192.0) / 8192.0
}

/// A MIDI 2.0 pitch bend (centre [`BendValue::CENTER`]) → `[-1.0, 1.0]`.
#[inline]
pub fn bend_to_signed_f32(v: BendValue) -> f32 {
    ((v.get() as f64 - 0x8000_0000_u32 as f64) / 0x8000_0000_u32 as f64) as f32
}

/// `[0.0, 1.0]` → a MIDI 2.0 velocity. Inverse of [`velocity_to_unit_f32`];
/// clamps.
#[inline]
pub fn unit_f32_to_velocity(v: f32) -> NoteVelocity {
    NoteVelocity::new((v.clamp(0.0, 1.0) as f64 * u16::MAX as f64).round() as u16)
}

/// `[0.0, 1.0]` → a MIDI 2.0 controller value. Inverse of
/// [`value_to_unit_f32`]; clamps.
#[inline]
pub fn unit_f32_to_value(v: f32) -> ControlValue {
    ControlValue::new((v.clamp(0.0, 1.0) as f64 * u32::MAX as f64).round() as u32)
}

/// `[-1.0, 1.0]` → a MIDI 2.0 pitch bend. Inverse of [`bend_to_signed_f32`];
/// clamps and saturates at the 32-bit max.
#[inline]
pub fn signed_f32_to_bend(v: f32) -> BendValue {
    let centered = 0x8000_0000_u32 as f64 + v.clamp(-1.0, 1.0) as f64 * 0x8000_0000_u32 as f64;
    BendValue::new(centered.round().clamp(0.0, u32::MAX as f64) as u32)
}
