//! Per-note pitch-bend sensitivity as the MIDI 2.0 wire value.

/// Per-note pitch bend range as the spec's 7.25 fixed-point semitone value
/// (M2-104 §7.4.13, RPN #00/07 "Sensitivity of Per-Note Pitch Bend"): 7 integer
/// bits of semitones in the high word, 25 fractional bits below. This is the wire
/// form the MPE Configuration Message (RPN 0x0006) carries — modelling it as the
/// fixed-point value, not a bare `u8`, is what lets it round-trip through UMP.
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
    const FRAC_BITS: u32 = 25;

    /// The MPE default per-note bend range: 48 semitones (MPE spec RP-053).
    pub const MPE_DEFAULT: Self = Self(48 << Self::FRAC_BITS);

    /// Whole-semitone sensitivity (the common MPE case, e.g. 48).
    #[inline]
    pub const fn from_semitones(semitones: u8) -> Self {
        Self((semitones as u32) << Self::FRAC_BITS)
    }

    /// The raw 32-bit RPN data field.
    #[inline]
    pub const fn to_rpn_bits(self) -> u32 {
        self.0
    }

    /// From the raw 32-bit RPN data field.
    #[inline]
    pub const fn from_rpn_bits(bits: u32) -> Self {
        Self(bits)
    }

    /// Sensitivity in semitones as a float (integer + fractional parts).
    #[inline]
    pub fn as_semitones_f32(self) -> f32 {
        self.0 as f32 / (1u32 << Self::FRAC_BITS) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pitch_bend_sensitivity_rpn_roundtrip() {
        for s in [2u8, 24, 48, 96] {
            let sens = PitchBendSensitivity::from_semitones(s);
            assert_eq!(
                PitchBendSensitivity::from_rpn_bits(sens.to_rpn_bits()),
                sens
            );
            assert_eq!(sens.as_semitones_f32(), f32::from(s));
        }
    }
}
