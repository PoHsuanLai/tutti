//! The downscale of M2-115 section 3.4: a plain right shift.

use tutti_ump::{U7, U14};

/// A 16-bit Note On or Note Off velocity as 7 bits: the top 7 bits.
///
/// A nonzero velocity below `0x0200` narrows to 0, which MIDI 1.0 reads as a
/// Note Off. A caller that sends a Note On on must not send that 0.
#[inline]
#[must_use]
pub fn narrow_velocity(velocity: u16) -> U7 {
    // `>> 9` leaves 7 bits, so the fallback is never taken.
    U7::new((velocity >> 9) as u8).unwrap_or(U7::MAX)
}

/// A 32-bit controller or pressure value as 7 bits: the top 7 bits.
#[inline]
#[must_use]
pub fn narrow_value(value: u32) -> U7 {
    // `>> 25` leaves 7 bits, so the fallback is never taken.
    U7::new((value >> 25) as u8).unwrap_or(U7::MAX)
}

/// A 32-bit Pitch Bend as 14 bits: the top 14 bits.
#[inline]
#[must_use]
pub fn narrow_bend(bend: u32) -> U14 {
    // `>> 18` leaves 14 bits, so the fallback is never taken.
    U14::new((bend >> 18) as u16).unwrap_or(U14::MAX)
}
