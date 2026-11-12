//! A native UMP integer as the unit float a DSP node wants, and back.
//!
//! These map a native UMP integer to the `[0.0, 1.0]` / `[-1.0, 1.0]` range a
//! DSP node wants. They are a lossy convenience for the audio edge, not part of
//! the MIDI 1.0 to 2.0 resolution path (`widen` and the `narrow_*` family):
//! `u32_to_unit_f32` loses low bits through f32's 24-bit mantissa. Consumers
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

/// 7-bit value (0-127) → `[0.0, 1.0]`.
#[inline]
pub fn u7_to_unit_f32(v: u8) -> f32 {
    f32::from(v) / 127.0
}

/// 16-bit value → `[0.0, 1.0]`.
#[inline]
pub fn u16_to_unit_f32(v: u16) -> f32 {
    f32::from(v) / f32::from(u16::MAX)
}

/// 32-bit value → `[0.0, 1.0]`.
#[inline]
pub fn u32_to_unit_f32(v: u32) -> f32 {
    (v as f64 / u32::MAX as f64) as f32
}

/// 14-bit pitch bend (center 8192) → `[-1.0, 1.0]`.
#[inline]
pub fn bend_u14_to_signed_f32(v: u16) -> f32 {
    (f32::from(v) - 8192.0) / 8192.0
}

/// 32-bit pitch bend (center 0x8000_0000) → `[-1.0, 1.0]`.
#[inline]
pub fn bend_u32_to_signed_f32(v: u32) -> f32 {
    ((v as f64 - 0x8000_0000_u32 as f64) / 0x8000_0000_u32 as f64) as f32
}

/// `[0.0, 1.0]` → 16-bit value. Inverse of [`u16_to_unit_f32`]; clamps.
#[inline]
pub fn unit_f32_to_u16(v: f32) -> u16 {
    (v.clamp(0.0, 1.0) as f64 * u16::MAX as f64).round() as u16
}

/// `[0.0, 1.0]` → 32-bit value. Inverse of [`u32_to_unit_f32`]; clamps.
#[inline]
pub fn unit_f32_to_u32(v: f32) -> u32 {
    (v.clamp(0.0, 1.0) as f64 * u32::MAX as f64).round() as u32
}

/// `[-1.0, 1.0]` → 32-bit pitch bend (center 0x8000_0000). Inverse of
/// [`bend_u32_to_signed_f32`]; clamps and saturates at the 32-bit max.
#[inline]
pub fn signed_f32_to_bend_u32(v: f32) -> u32 {
    let centered = 0x8000_0000_u32 as f64 + v.clamp(-1.0, 1.0) as f64 * 0x8000_0000_u32 as f64;
    centered.round().clamp(0.0, u32::MAX as f64) as u32
}
