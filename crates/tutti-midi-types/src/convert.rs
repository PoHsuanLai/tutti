//! MIDI 1.0 ↔ MIDI 2.0 scalar resolution conversion (spec Min-Center-Max).
//!
//! `midi2` applies the same scaling inside its `HybridSchemaProperty`
//! conversions when typed messages are rebuffered between 7-bit and UMP
//! forms. These free functions cover places where a tutti consumer has a
//! raw 7-bit value (from a hardware port, a DAW automation knob, or a test
//! helper) and wants the 16/32-bit UMP value to pass into a constructor like
//! [`MidiEvent::note_on`](crate::ump::MidiEvent::note_on).

/// 7-bit velocity → 16-bit velocity (MIDI 2.0 native form).
#[inline]
pub fn midi1_velocity_to_midi2(v: u8) -> u16 {
    if v == 0 {
        0
    } else {
        let v7 = u32::from(v);
        ((v7 * 65535 + 63) / 127) as u16
    }
}

/// 16-bit velocity → 7-bit (lossy).
#[inline]
pub fn midi2_velocity_to_midi1(v: u16) -> u8 {
    if v == 0 {
        0
    } else {
        let v16 = u32::from(v);
        ((v16 * 127 + 32767) / 65535).min(127) as u8
    }
}

/// 7-bit CC / pressure → 32-bit (MIDI 2.0 native form).
#[inline]
pub fn midi1_cc_to_midi2(v: u8) -> u32 {
    if v == 0 {
        0
    } else if v == 127 {
        u32::MAX
    } else {
        let v7 = u64::from(v);
        ((v7 * u32::MAX as u64 + 63) / 127) as u32
    }
}

/// 32-bit CC / pressure → 7-bit (lossy).
#[inline]
pub fn midi2_cc_to_midi1(v: u32) -> u8 {
    if v == 0 {
        0
    } else {
        let v32 = u64::from(v);
        ((v32 * 127 + 0x7FFF_FFFF) / u32::MAX as u64).min(127) as u8
    }
}

/// 14-bit pitch bend (center 8192) → 32-bit (center 0x8000_0000).
#[inline]
pub fn midi1_pitch_bend_to_midi2(v: u16) -> u32 {
    if v == 0 {
        0
    } else if v == 16383 {
        u32::MAX
    } else {
        let v14 = u64::from(v);
        ((v14 * u32::MAX as u64 + 8191) / 16383) as u32
    }
}

/// 32-bit pitch bend → 14-bit (lossy).
#[inline]
pub fn midi2_pitch_bend_to_midi1(v: u32) -> u16 {
    if v == 0 {
        0
    } else {
        let v32 = u64::from(v);
        ((v32 * 16383 + 0x7FFF_FFFF) / u32::MAX as u64).min(16383) as u16
    }
}

// --- UMP integer → canonical f32 unit range -----------------------------------
//
// Unsigned values map to `[0.0, 1.0]`; pitch bends map to `[-1.0, 1.0]` around
// their center code. Float-domain counterpart to the integer converters above —
// semantic decoding, MPE, and CC handling all normalize through here.

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn velocity_roundtrip() {
        for v in 0..=127u8 {
            assert_eq!(midi2_velocity_to_midi1(midi1_velocity_to_midi2(v)), v);
        }
    }

    #[test]
    fn cc_roundtrip() {
        for v in 0..=127u8 {
            assert_eq!(midi2_cc_to_midi1(midi1_cc_to_midi2(v)), v);
        }
    }

    #[test]
    fn pitch_bend_roundtrip() {
        for v in 0..=16383u16 {
            assert_eq!(midi2_pitch_bend_to_midi1(midi1_pitch_bend_to_midi2(v)), v);
        }
    }

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
}
