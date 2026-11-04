//! The MIDI 2.0 fixed-point pitch encodings as floats.
//!
//! M2-104 carries a pitch (a note number with a fraction of a semitone) and a
//! per-note bend range as unsigned fixed-point words: 7 integer bits, then the
//! fraction. These are the one conversion each of them has.

/// Fraction bits of the 32-bit pitch word (Registered Per-Note Controller #3,
/// M2-104 §7.4.15.2) and of the per-note bend sensitivity (RPN #00/07,
/// §7.4.13): Q7.25.
pub(crate) const Q7_25_FRAC_BITS: u32 = 25;

/// A Q7.25 word (7 integer bits, 25 fractional bits) as a float: a fractional
/// MIDI note number, or a range in semitones.
///
/// ```
/// use tutti_midi_types::q7_25_to_f32;
///
/// assert_eq!(q7_25_to_f32(69 << 25), 69.0);
/// assert_eq!(q7_25_to_f32(1 << 24), 0.5);
/// ```
#[inline]
pub fn q7_25_to_f32(bits: u32) -> f32 {
    bits as f32 / (1u32 << Q7_25_FRAC_BITS) as f32
}

/// A Q7.9 word (7 integer bits, 9 fractional bits; Note-On Attribute #3,
/// M2-104 §7.4.15.3) as a fractional MIDI note number.
///
/// ```
/// use tutti_midi_types::q7_9_to_f32;
///
/// assert_eq!(q7_9_to_f32(60 << 9), 60.0);
/// assert_eq!(q7_9_to_f32((60 << 9) | 256), 60.5);
/// ```
#[inline]
pub fn q7_9_to_f32(bits: u16) -> f32 {
    bits as f32 / (1u16 << 9) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Analytic: the integer part is the high 7 bits, the fraction the rest.
    ///
    /// Mutations (each fails): dividing by `1 << 24`; dividing by `1 << 8` in
    /// the Q7.9 case.
    #[test]
    fn integer_and_fraction_split() {
        assert_eq!(q7_25_to_f32(0), 0.0);
        assert_eq!(q7_25_to_f32(127 << 25), 127.0);
        assert_eq!(q7_25_to_f32((12 << 25) | (1 << 24)), 12.5);
        assert_eq!(q7_25_to_f32((12 << 25) | (1 << 23)), 12.25);
        assert_eq!(q7_9_to_f32(0), 0.0);
        assert_eq!(q7_9_to_f32((127 << 9) | 128), 127.25);
    }
}
