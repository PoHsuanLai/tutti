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
/// Round-trips through [`velocity_from_midi2`] to within one 16-bit code.
///
/// # What this trades
///
/// It no longer reproduces [`midi1_velocity_to_midi2`] at every 7-bit input.
/// Away from the three fixed points the two differ by up to 258 codes — 0.4% of
/// full scale, well under a JND for velocity, and no caller compares them. What
/// is preserved exactly is what a listener or a device can actually notice:
/// `0.0`, `0.5` and `1.0` land on `0x0000`, `0x8000` and `0xffff`, so
/// [`Velocity::CENTER`] and a hardware `64` still agree.
///
/// **Promoting an actual MIDI 1.0 message is a different path and is
/// unaffected** — `translation::promote` calls [`midi1_velocity_to_midi2`]
/// directly on the `u8`, never through here, so a 7-bit wire value still widens
/// by exact MCM. This function is only for values that start as a `Velocity`,
/// where there is no 7-bit original to be faithful to.
///
/// # This used to quantize to 7 bits
///
/// It was `midi1_velocity_to_midi2(velocity_to_midi1(v))` — via the 7-bit rung,
/// because that rung's center is exactly representable and a plain `* 65535.0`
/// misses `0x8000`. The center property was real; the cost was not noticed.
/// Routing through `u8` meant a `Velocity` could only ever produce **128
/// distinct 16-bit codes**, so the float-backed type — which exists precisely
/// because the wire field is 16-bit — was narrowed to the resolution it was
/// introduced to escape. Every note-on in the engine went through this.
///
/// The two directions were also asymmetric (`to` via MCM, `from` a plain
/// divide), which is what let the loss hide: a round trip of any *7-bit* value
/// is exact, and those are the only values the old path could emit, so it was
/// self-consistently lossy.
///
/// # The mapping is piecewise, because MCM is
///
/// MCM widening is not a uniform scale. Its two halves meet at the center with a
/// one-code step — 7-bit `63` widens to `0x7fff` and `64` to `0x8000` — so the
/// float `0.5` sits in a *gap* on the widened lattice, not on a lattice point.
/// Interpolating uniformly across that gap lands at `0x7f00` and loses the exact
/// center, which is the property the old 7-bit path was protecting.
///
/// So each half is scaled independently against the anchor it belongs to:
/// `0.0 → 0x0000`, `0.5 → 0x8000`, `1.0 → 0xffff`. That is the spec's own
/// convention (§ MIDI 2.0 velocity is unsigned with center at `0x8000`) and it
/// makes the three fixed points exact by construction rather than by arithmetic
/// luck.
///
/// [`Velocity`]: tutti_types::Velocity
/// [`Velocity::CENTER`]: tutti_types::Velocity::CENTER
/// [`MidiEvent::note_on`]: crate::ump::MidiEvent::note_on
#[inline]
pub fn velocity_to_midi2(v: tutti_types::Velocity) -> u16 {
    const CENTER: f32 = 0x8000 as f32;
    let v = v.get().clamp(0.0, 1.0);
    if v <= 0.5 {
        // Lower half: 0.0 → 0x0000, 0.5 → 0x8000.
        (v * 2.0 * CENTER).round() as u16
    } else {
        // Upper half: 0.5 → 0x8000, 1.0 → 0xffff. The span is one code shorter
        // than the lower half, which is exactly the asymmetry MCM encodes.
        (CENTER + (v - 0.5) * 2.0 * (f32::from(u16::MAX) - CENTER)).round() as u16
    }
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

/// A 16-bit MIDI 2.0 velocity → [`Velocity`]. The exact inverse of
/// [`velocity_to_midi2`].
///
/// Piecewise for the same reason that one is: a plain `v / 0xffff` maps the
/// center code `0x8000` to `0.5000076`, so `CENTER → 0x8000 → 0.5000076` and the
/// round trip does not close on the one value most likely to be compared. The
/// old pair got away with it because the forward direction could only emit 7-bit
/// codes, where the error is below the grid; restoring full resolution makes the
/// mismatch reachable.
///
/// [`Velocity`]: tutti_types::Velocity
#[inline]
pub fn velocity_from_midi2(v: u16) -> tutti_types::Velocity {
    const CENTER: f32 = 0x8000 as f32;
    let v = f32::from(v);
    tutti_types::Velocity(if v <= CENTER {
        v / (2.0 * CENTER)
    } else {
        0.5 + (v - CENTER) / (2.0 * (f32::from(u16::MAX) - CENTER))
    })
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
    /// **A `Velocity` reaches the wire at 16-bit resolution, not 7.**
    ///
    /// This regressed once and was invisible: `velocity_to_midi2` routed through
    /// the 7-bit rung (`midi1_velocity_to_midi2(velocity_to_midi1(v))`) to keep
    /// the center exact, which capped the whole float type at **128 distinct
    /// codes**. `Velocity` is float-backed *precisely because* the wire field is
    /// 16-bit, so that silently undid the reason the type exists.
    ///
    /// It hid because the two directions were asymmetric — `to` went via MCM,
    /// `from` was a plain divide — so a round trip of any 7-bit value was exact,
    /// and those were the only values the path could emit. It was
    /// self-consistently lossy.
    #[test]
    fn a_velocity_is_not_quantized_to_seven_bits() {
        use std::collections::BTreeSet;
        let codes: BTreeSet<u16> = (0..=1000)
            .map(|i| velocity_to_midi2(tutti_types::Velocity(i as f32 / 1000.0)))
            .collect();
        assert!(
            codes.len() > 900,
            "1001 distinct inputs collapsed to {} codes; 128 means the 7-bit rung is back",
            codes.len()
        );
    }

    /// The three fixed points stay exact — the property the 7-bit rung existed
    /// to protect, and the reason a plain `* 65535.0` is wrong.
    #[test]
    fn velocity_endpoints_and_centre_are_exact() {
        use tutti_types::Velocity;
        assert_eq!(velocity_to_midi2(Velocity::SILENT), 0x0000);
        assert_eq!(velocity_to_midi2(Velocity::CENTER), 0x8000, "centre must be mezzo-forte");
        assert_eq!(velocity_to_midi2(Velocity::MAX), u16::MAX);
    }

    /// A `Velocity` survives a round trip far better than the 7-bit grid allows.
    #[test]
    fn a_velocity_round_trips_within_one_code() {
        for i in 0..=1000 {
            let v = i as f32 / 1000.0;
            let back = velocity_from_midi2(velocity_to_midi2(tutti_types::Velocity(v))).get();
            assert!(
                (back - v).abs() < 1e-4,
                "round trip of {v} returned {back}; the 7-bit grid would be 0.0078 off"
            );
        }
    }

    /// Promoting a real MIDI 1.0 velocity is untouched: it widens by exact MCM,
    /// because there the 7-bit value *is* the original and must be reproduced.
    #[test]
    fn a_seven_bit_wire_velocity_still_widens_by_exact_mcm() {
        assert_eq!(midi1_velocity_to_midi2(0), 0x0000);
        assert_eq!(midi1_velocity_to_midi2(64), 0x8000);
        assert_eq!(midi1_velocity_to_midi2(127), 0xffff);
        // And the inverse is exact for every code.
        for c in 0u8..=127 {
            assert_eq!(midi2_velocity_to_midi1(midi1_velocity_to_midi2(c)), c);
        }
    }
    /// The two directions are inverses at the center, not merely near it.
    ///
    /// `velocity_from_midi2` was a plain `v / 0xffff`, which maps `0x8000` to
    /// `0.5000076` — so `CENTER` did not survive a round trip. The old forward
    /// path hid it by only ever emitting 7-bit codes, where the error is below
    /// the grid. Restoring resolution made it reachable, so both halves are now
    /// piecewise about the same anchor.
    #[test]
    fn the_two_velocity_directions_are_inverses() {
        use tutti_types::Velocity;
        for v in [Velocity::SILENT, Velocity::CENTER, Velocity::MAX] {
            assert_eq!(velocity_from_midi2(velocity_to_midi2(v)), v);
        }
        for code in [0u16, 0x4000, 0x8000, 0xC000, 0xffff] {
            assert_eq!(velocity_to_midi2(velocity_from_midi2(code)), code);
        }
    }
}
