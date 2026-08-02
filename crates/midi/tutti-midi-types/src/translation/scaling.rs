//! **Bit Scaling and Resolution** (M2-104 §1.7 / Appendix D.1) — the
//! Min-Center-Max scaling of a scalar value between the MIDI 1.0 and MIDI 2.0
//! protocols' bit widths.
//!
//! The MIDI 2.0 spec (M2-104 Appendix D.1.3, "Default Upscaling Method") widens
//! a value by left-shifting into the high bits, then *bit-repeating* the source's
//! fractional bits for values above center. This is what makes min→min,
//! center→center, and max→max map exactly — a plain linear scale does not
//! preserve the center code (14-bit bend `8192` must widen to `0x8000_0000`, not
//! a value one ULP off). Downscaling is the symmetric truncation.
//!
//! These free functions cover places where a tutti consumer has a raw 7/14-bit
//! value (from a hardware port, a DAW automation knob, or a test helper) and
//! wants the 16/32-bit UMP value to pass into a constructor like
//! [`MidiEvent::note_on`](crate::ump::MidiEvent::note_on).

/// Spec Min-Center-Max upscale: shift into the high `dst - src` bits, then
/// bit-repeat the source's fractional bits for values above center. `dst >= src`,
/// both ≤ 32.
const fn mcm_up(value: u32, src: u32, dst: u32) -> u32 {
    let shift = dst - src;
    let shifted = value << shift;
    let center = 1u32 << (src - 1);
    if value <= center {
        return shifted; // lower half (incl. center) is a pure left-shift
    }
    let repeat_bits = src - 1;
    let mut repeat = value & ((1u32 << repeat_bits) - 1);
    repeat = if shift >= repeat_bits {
        repeat << (shift - repeat_bits)
    } else {
        repeat >> (repeat_bits - shift)
    };
    let mut out = shifted;
    while repeat != 0 {
        out |= repeat;
        repeat >>= repeat_bits;
    }
    out
}

/// Spec Min-Center-Max downscale: keep the top `dst` bits (drop the repeated
/// tail). Exact inverse of [`mcm_up`] for every representable source value.
const fn mcm_down(value: u32, src: u32, dst: u32) -> u32 {
    value >> (src - dst)
}

/// 7-bit velocity → 16-bit velocity (MIDI 2.0 native form).
#[inline]
pub fn midi1_velocity_to_midi2(v: u8) -> u16 {
    mcm_up(v as u32, 7, 16) as u16
}

/// [`Velocity`] → the 16-bit field [`MidiEvent::note_on`] takes.
///
/// Lives here rather than on `Velocity` itself because the widening is the
/// spec's Min-Center-Max scaler, not a multiply: `0.5` must land on `0x8000`
/// exactly, and a plain `* 65535.0` misses it. `tutti-types` cannot host this —
/// it does not depend on this crate, and copying [`mcm_up`] there would be a
/// second home for a spec algorithm, the thing the ITU downmix coefficients are
/// kept single-homed to avoid.
///
/// Round-trips through [`velocity_from_midi2`] for every 7-bit input.
///
/// [`Velocity`]: tutti_types::Velocity
/// [`MidiEvent::note_on`]: crate::ump::MidiEvent::note_on
#[inline]
pub fn velocity_to_midi2(v: tutti_types::Velocity) -> u16 {
    // Via the 7-bit rung on purpose: it is the only path whose center is
    // exactly representable, so `Velocity::CENTER` and a hardware `64` produce
    // the identical 16-bit code rather than two values one ULP apart.
    midi1_velocity_to_midi2(velocity_to_midi1(v))
}

/// [`Velocity`] → a 7-bit MIDI 1.0 velocity, for the wire and for
/// [`MidiEvent::note_on_7bit`].
///
/// [`Velocity`]: tutti_types::Velocity
/// [`MidiEvent::note_on_7bit`]: crate::ump::MidiEvent::note_on_7bit
#[inline]
pub fn velocity_to_midi1(v: tutti_types::Velocity) -> u8 {
    // `round`, not `as`: truncation maps 1.0 to 126 and loses the top code.
    (v.get().clamp(0.0, 1.0) * 127.0).round() as u8
}

/// A 7-bit wire velocity → [`Velocity`]. The one sanctioned `/ 127.0`.
///
/// [`Velocity`]: tutti_types::Velocity
#[inline]
pub fn velocity_from_midi1(v: u8) -> tutti_types::Velocity {
    tutti_types::Velocity((v & 0x7F) as f32 / 127.0)
}

/// A 16-bit MIDI 2.0 velocity → [`Velocity`].
///
/// [`Velocity`]: tutti_types::Velocity
#[inline]
pub fn velocity_from_midi2(v: u16) -> tutti_types::Velocity {
    tutti_types::Velocity(v as f32 / u16::MAX as f32)
}

/// 16-bit velocity → 7-bit (lossy).
#[inline]
pub fn midi2_velocity_to_midi1(v: u16) -> u8 {
    mcm_down(v as u32, 16, 7) as u8
}

/// 7-bit CC / pressure → 32-bit (MIDI 2.0 native form).
#[inline]
pub fn midi1_cc_to_midi2(v: u8) -> u32 {
    mcm_up(v as u32, 7, 32)
}

/// 32-bit CC / pressure → 7-bit (lossy).
#[inline]
pub fn midi2_cc_to_midi1(v: u32) -> u8 {
    mcm_down(v, 32, 7) as u8
}

/// 14-bit pitch bend (center 8192) → 32-bit (center 0x8000_0000).
#[inline]
pub fn midi1_pitch_bend_to_midi2(v: u16) -> u32 {
    mcm_up(v as u32, 14, 32)
}

/// 32-bit pitch bend → 14-bit (lossy).
#[inline]
pub fn midi2_pitch_bend_to_midi1(v: u32) -> u16 {
    mcm_down(v, 32, 14) as u16
}

// --- UMP integer → f32 unit range (DSP normalization, NOT MIDI1↔2 scaling) ----
//
// These map a native UMP integer to the `[0.0, 1.0]` / `[-1.0, 1.0]` range a DSP
// node wants. They are a lossy convenience for the audio edge — NOT part of the
// MIDI 1.0 ↔ 2.0 resolution path above (`u32_to_unit_f32` loses low bits through
// f32's 24-bit mantissa). Consumers call these at their own boundary, not through
// a shared decode layer.

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

    /// A 7-bit wire velocity survives the trip through [`Velocity`] unchanged.
    ///
    /// The property that matters at an input edge: a controller sending 100 and
    /// a document storing that note must produce the same 100 on the way out,
    /// or every save/load cycle drifts the dynamics.
    #[test]
    fn a_7bit_velocity_round_trips_through_the_unit() {
        for v in 0u8..=127 {
            assert_eq!(velocity_to_midi1(velocity_from_midi1(v)), v, "velocity {v}");
        }
    }

    /// The endpoints and the center land exactly.
    ///
    /// `0.5 * 65535.0` is `32767.5` — it does *not* give `0x8000`, which is why
    /// the conversion routes through the spec scaler rather than multiplying.
    #[test]
    fn velocity_endpoints_and_center_are_exact() {
        use tutti_types::Velocity;
        assert_eq!(velocity_to_midi2(Velocity::SILENT), 0);
        assert_eq!(velocity_to_midi2(Velocity::MAX), 0xffff);
        assert_eq!(velocity_to_midi2(Velocity::CENTER), 0x8000);
        // And `note_on_7bit`'s path agrees with the widened one, so the two
        // constructors cannot disagree about the same note.
        assert_eq!(
            velocity_to_midi2(Velocity::CENTER),
            midi1_velocity_to_midi2(velocity_to_midi1(Velocity::CENTER))
        );
    }

    /// Out-of-range input is clamped, not wrapped.
    ///
    /// `as u8` on a value above 255 wraps, which would turn the loudest
    /// possible note into a near-silent one.
    #[test]
    fn an_out_of_range_velocity_clamps() {
        use tutti_types::Velocity;
        assert_eq!(velocity_to_midi1(Velocity(1.5)), 127);
        assert_eq!(velocity_to_midi1(Velocity(-0.5)), 0);
    }

    #[test]
    fn spec_min_center_max_vectors() {
        // M2-104 Appendix D.1.3 worked examples — a linear scale fails these.
        assert_eq!(midi1_velocity_to_midi2(10), 0x1400);
        assert_eq!(midi1_velocity_to_midi2(64), 0x8000); // center → center
        assert_eq!(midi1_velocity_to_midi2(87), 0xaeba);
        assert_eq!(midi1_velocity_to_midi2(127), 0xffff);
        assert_eq!(midi1_pitch_bend_to_midi2(8192), 0x8000_0000); // bend center
        assert_eq!(midi1_cc_to_midi2(127), 0xffff_ffff);
        assert_eq!(midi1_cc_to_midi2(0), 0);
    }

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
