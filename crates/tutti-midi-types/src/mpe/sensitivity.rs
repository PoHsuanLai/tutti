//! Per-note pitch-bend sensitivity as the MIDI 2.0 wire value.

/// Per-note pitch bend range as the spec's 7.25 fixed-point semitone value
/// (M2-104 §7.4.13, RPN #00/07 "Sensitivity of Per-Note Pitch Bend"): 7 integer
/// bits of semitones in the high word, 25 fractional bits below. This is the wire
/// form the MPE Configuration Message (RPN 0x0006) carries — modelling it as the
/// fixed-point value, not a bare `u8`, is what lets it round-trip through UMP.
///
/// Every `u32` is a sensitivity (§7.4.13.3: just under 128 semitones; the top
/// words round to 128.0 in `f32`), so the bits have no refused value; only a
/// whole-semitone count can be out of range ([`new`](Self::new), [`of`](Self::of)).
/// Serde carries the bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PitchBendSensitivity(u32);

impl Default for PitchBendSensitivity {
    /// The MPE default per-note bend range: ±48 semitones (MPE spec RP-053).
    #[inline]
    fn default() -> Self {
        Self::MPE_DEFAULT
    }
}

impl PitchBendSensitivity {
    const FRAC_BITS: u32 = crate::fixed::Q7_25_FRAC_BITS;

    /// The MPE default per-note bend range: 48 semitones (MPE spec RP-053).
    pub const MPE_DEFAULT: Self = Self(48 << Self::FRAC_BITS);

    /// The widest whole-semitone sensitivity: 7 integer bits.
    pub const MAX_SEMITONES: u8 = 127;

    /// Whole-semitone sensitivity (the common MPE case, e.g. 48), or `None`
    /// above [`MAX_SEMITONES`](Self::MAX_SEMITONES). A `u8` count, not
    /// `Semitones`: that is an `f32`, and this is an exact integer count.
    #[inline]
    #[must_use]
    pub const fn new(semitones: u8) -> Option<Self> {
        if semitones <= Self::MAX_SEMITONES {
            Some(Self((semitones as u32) << Self::FRAC_BITS))
        } else {
            None
        }
    }

    /// [`new`](Self::new) for a literal or a constant.
    ///
    /// # Panics
    ///
    /// Above [`MAX_SEMITONES`](Self::MAX_SEMITONES).
    #[inline]
    #[must_use]
    #[track_caller]
    pub const fn of(semitones: u8) -> Self {
        match Self::new(semitones) {
            Some(s) => s,
            None => panic!("a pitch bend sensitivity is at most 127 semitones"),
        }
    }

    /// The raw 32-bit RPN data field.
    #[inline]
    #[must_use]
    pub const fn to_rpn_bits(self) -> u32 {
        self.0
    }

    /// From the raw 32-bit RPN data field. Total: every value is in range.
    #[inline]
    #[must_use]
    pub const fn from_rpn_bits(bits: u32) -> Self {
        Self(bits)
    }

    /// Sensitivity in semitones as a float (integer + fractional parts).
    #[inline]
    #[must_use]
    pub fn as_semitones_f32(self) -> f32 {
        crate::q7_25_to_f32(self.0)
    }
}

#[cfg(test)]
mod tests;
