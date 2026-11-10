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
//! a `Voice2::NoteOn` (`tutti_ump::Voice2`).

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
///
/// Retiring: no caller outside the plugin crates (doc 017 stage 5b); use tutti_midi_protocol::widen (a Voice1 Note Off or Note On).
#[inline]
pub fn midi1_velocity_to_midi2(v: u8) -> u16 {
    mcm_up(v as u32, 7, 16) as u16
}

/// [`Velocity`] → the 16-bit field a `Voice2::NoteOn` takes.
///
/// Lives here rather than on `Velocity` itself because the widening is the
/// spec's Min-Center-Max scaler, not a multiply: `0.5` must land on `0x8000`
/// exactly, and a plain `* 65535.0` misses it. `tutti-types` cannot host this —
/// it does not depend on this crate, and copying `mcm_up` there would be a
/// second home for a spec algorithm, the thing the ITU downmix coefficients are
/// kept single-homed to avoid.
///
/// Round-trips through [`velocity_from_midi2`] to within one 16-bit code.
///
/// # What this trades
///
/// It does not reproduce [`midi1_velocity_to_midi2`] at every 7-bit input.
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
/// # Do not route this through the 7-bit rung
///
/// `midi1_velocity_to_midi2(velocity_to_midi1(v))` is the tempting
/// implementation, because the 7-bit rung's center is exactly representable
/// where a plain `* 65535.0` misses `0x8000`. It buys that one property by
/// capping a `Velocity` at **128 distinct 16-bit codes** — narrowing the
/// float-backed type to exactly the resolution it exists to escape, on every
/// note-on in the engine.
///
/// The loss hides because such a path is self-consistently lossy: a round trip
/// of any *7-bit* value is exact, and those are the only values it can emit.
///
/// # The mapping is piecewise, because MCM is
///
/// MCM widening is not a uniform scale. Its two halves meet at the center with a
/// one-code step — 7-bit `63` widens to `0x7fff` and `64` to `0x8000` — so the
/// float `0.5` sits in a *gap* on the widened lattice, not on a lattice point.
/// Interpolating uniformly across that gap lands at `0x7f00` and loses the exact
/// center, which a 7-bit path would keep.
///
/// So each half is scaled independently against the anchor it belongs to:
/// `0.0 → 0x0000`, `0.5 → 0x8000`, `1.0 → 0xffff`. That is the spec's own
/// convention (§ MIDI 2.0 velocity is unsigned with center at `0x8000`) and it
/// makes the three fixed points exact by construction rather than by arithmetic
/// luck.
///
/// [`Velocity`]: tutti_types::Velocity
/// [`Velocity::CENTER`]: tutti_types::Velocity::CENTER
///
/// Retiring: no caller outside the plugin crates (doc 017 stage 5b); use the spec's 16-bit value written directly.
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
/// a 7-bit MIDI 1.0 note.
///
/// [`Velocity`]: tutti_types::Velocity
///
/// Retiring: no caller outside the plugin crates (doc 017 stage 5b); use tutti_midi_protocol::narrow_velocity on a 16-bit value.
#[inline]
pub fn velocity_to_midi1(v: tutti_types::Velocity) -> u8 {
    // `round`, not `as`: truncation maps 1.0 to 126 and loses the top code.
    (v.get().clamp(0.0, 1.0) * 127.0).round() as u8
}

/// A 7-bit wire velocity → [`Velocity`]. The one sanctioned `/ 127.0`.
///
/// [`Velocity`]: tutti_types::Velocity
///
/// Retiring: no caller outside the plugin crates (doc 017 stage 5b); use the 7-bit value as a unit float (u7_to_unit_f32).
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
///
/// Retiring: no caller outside the plugin crates (doc 017 stage 5b); use u16_to_unit_f32.
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
///
/// Retiring: no caller outside the plugin crates (doc 017 stage 5b); use tutti_midi_protocol::narrow_velocity.
#[inline]
pub fn midi2_velocity_to_midi1(v: u16) -> u8 {
    mcm_down(v as u32, 16, 7) as u8
}

/// 7-bit CC / pressure → 32-bit (MIDI 2.0 native form).
///
/// Retiring: no caller outside the plugin crates (doc 017 stage 5b); use tutti_midi_protocol::widen (a Voice1 Control Change).
#[inline]
pub fn midi1_cc_to_midi2(v: u8) -> u32 {
    mcm_up(v as u32, 7, 32)
}

/// 32-bit CC / pressure → 7-bit (lossy).
///
/// Retiring: no caller outside the plugin crates (doc 017 stage 5b); use tutti_midi_protocol::narrow_value.
#[inline]
pub fn midi2_cc_to_midi1(v: u32) -> u8 {
    mcm_down(v, 32, 7) as u8
}

/// 14-bit pitch bend (center 8192) → 32-bit (center 0x8000_0000).
///
/// Retiring: no caller outside the plugin crates (doc 017 stage 5b); use tutti_midi_protocol::widen (a Voice1 Pitch Bend).
#[inline]
pub fn midi1_pitch_bend_to_midi2(v: u16) -> u32 {
    mcm_up(v as u32, 14, 32)
}

/// 32-bit pitch bend → 14-bit (lossy).
///
/// Retiring: no caller outside the plugin crates (doc 017 stage 5b); use tutti_midi_protocol::narrow_bend.
#[inline]
pub fn midi2_pitch_bend_to_midi1(v: u32) -> u16 {
    mcm_down(v, 32, 14) as u16
}

// The float scaling lives in `tutti-midi-protocol` (doc 017 stage 3d); these
// paths stay for the plugin crates.
pub use tutti_midi_protocol::{
    bend_u14_to_signed_f32, bend_u32_to_signed_f32, signed_f32_to_bend_u32, u7_to_unit_f32,
    u16_to_unit_f32, u32_to_unit_f32, unit_f32_to_u16, unit_f32_to_u32,
};

#[cfg(test)]
mod tests;
