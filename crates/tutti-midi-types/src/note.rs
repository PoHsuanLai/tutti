//! MIDI note constants and enum.
//!
//! Provides type-safe note representation with all 128 MIDI notes.
//!
//! # Example
//! ```ignore
//! use tutti_midi_types::Note;
//!
//! let middle_c = Note::C4;
//! let concert_a = Note::A4;
//!
//! assert_eq!(u8::from(Note::C4), 60);
//! assert_eq!(u8::from(Note::A4), 69);
//! ```

use core::fmt;

/// MIDI note number (0-127).
///
/// Notes are named using scientific pitch notation:
/// - Letter: C, D, E, F, G, A, B
/// - Accidental: s = sharp (e.g., `Cs4` = C#4)
/// - Octave: -1 to 9
///
/// Middle C (MIDI 60) is `C4`. Concert A (440 Hz, MIDI 69) is `A4`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Note {
    // Octave -1 (MIDI 0-11)
    Cm1 = 0,
    Csm1 = 1,
    Dm1 = 2,
    Dsm1 = 3,
    Em1 = 4,
    Fm1 = 5,
    Fsm1 = 6,
    Gm1 = 7,
    Gsm1 = 8,
    Am1 = 9,
    Asm1 = 10,
    Bm1 = 11,

    // Octave 0 (MIDI 12-23)
    C0 = 12,
    Cs0 = 13,
    D0 = 14,
    Ds0 = 15,
    E0 = 16,
    F0 = 17,
    Fs0 = 18,
    G0 = 19,
    Gs0 = 20,
    A0 = 21,
    As0 = 22,
    B0 = 23,

    // Octave 1 (MIDI 24-35)
    C1 = 24,
    Cs1 = 25,
    D1 = 26,
    Ds1 = 27,
    E1 = 28,
    F1 = 29,
    Fs1 = 30,
    G1 = 31,
    Gs1 = 32,
    A1 = 33,
    As1 = 34,
    B1 = 35,

    // Octave 2 (MIDI 36-47)
    C2 = 36,
    Cs2 = 37,
    D2 = 38,
    Ds2 = 39,
    E2 = 40,
    F2 = 41,
    Fs2 = 42,
    G2 = 43,
    Gs2 = 44,
    A2 = 45,
    As2 = 46,
    B2 = 47,

    // Octave 3 (MIDI 48-59)
    C3 = 48,
    Cs3 = 49,
    D3 = 50,
    Ds3 = 51,
    E3 = 52,
    F3 = 53,
    Fs3 = 54,
    G3 = 55,
    Gs3 = 56,
    A3 = 57,
    As3 = 58,
    B3 = 59,

    // Octave 4 (MIDI 60-71) - Middle C octave
    C4 = 60,
    Cs4 = 61,
    D4 = 62,
    Ds4 = 63,
    E4 = 64,
    F4 = 65,
    Fs4 = 66,
    G4 = 67,
    Gs4 = 68,
    A4 = 69, // Concert A (440 Hz)
    As4 = 70,
    B4 = 71,

    // Octave 5 (MIDI 72-83)
    C5 = 72,
    Cs5 = 73,
    D5 = 74,
    Ds5 = 75,
    E5 = 76,
    F5 = 77,
    Fs5 = 78,
    G5 = 79,
    Gs5 = 80,
    A5 = 81,
    As5 = 82,
    B5 = 83,

    // Octave 6 (MIDI 84-95)
    C6 = 84,
    Cs6 = 85,
    D6 = 86,
    Ds6 = 87,
    E6 = 88,
    F6 = 89,
    Fs6 = 90,
    G6 = 91,
    Gs6 = 92,
    A6 = 93,
    As6 = 94,
    B6 = 95,

    // Octave 7 (MIDI 96-107)
    C7 = 96,
    Cs7 = 97,
    D7 = 98,
    Ds7 = 99,
    E7 = 100,
    F7 = 101,
    Fs7 = 102,
    G7 = 103,
    Gs7 = 104,
    A7 = 105,
    As7 = 106,
    B7 = 107,

    // Octave 8 (MIDI 108-119)
    C8 = 108,
    Cs8 = 109,
    D8 = 110,
    Ds8 = 111,
    E8 = 112,
    F8 = 113,
    Fs8 = 114,
    G8 = 115,
    Gs8 = 116,
    A8 = 117,
    As8 = 118,
    B8 = 119,

    // Octave 9 (MIDI 120-127) - partial octave
    C9 = 120,
    Cs9 = 121,
    D9 = 122,
    Ds9 = 123,
    E9 = 124,
    F9 = 125,
    Fs9 = 126,
    G9 = 127,
}

impl Note {
    pub const MIDDLE_C: Note = Note::C4;
    pub const CONCERT_A: Note = Note::A4;

    /// Returns `None` if the value is > 127.
    pub const fn from_midi(midi: u8) -> Option<Note> {
        if midi > 127 {
            return None;
        }
        #[rustfmt::skip]
        const LUT: [Note; 128] = [
            Note::Cm1,  Note::Csm1, Note::Dm1,  Note::Dsm1, Note::Em1,  Note::Fm1,
            Note::Fsm1, Note::Gm1,  Note::Gsm1, Note::Am1,  Note::Asm1, Note::Bm1,
            Note::C0,   Note::Cs0,  Note::D0,   Note::Ds0,  Note::E0,   Note::F0,
            Note::Fs0,  Note::G0,   Note::Gs0,  Note::A0,   Note::As0,  Note::B0,
            Note::C1,   Note::Cs1,  Note::D1,   Note::Ds1,  Note::E1,   Note::F1,
            Note::Fs1,  Note::G1,   Note::Gs1,  Note::A1,   Note::As1,  Note::B1,
            Note::C2,   Note::Cs2,  Note::D2,   Note::Ds2,  Note::E2,   Note::F2,
            Note::Fs2,  Note::G2,   Note::Gs2,  Note::A2,   Note::As2,  Note::B2,
            Note::C3,   Note::Cs3,  Note::D3,   Note::Ds3,  Note::E3,   Note::F3,
            Note::Fs3,  Note::G3,   Note::Gs3,  Note::A3,   Note::As3,  Note::B3,
            Note::C4,   Note::Cs4,  Note::D4,   Note::Ds4,  Note::E4,   Note::F4,
            Note::Fs4,  Note::G4,   Note::Gs4,  Note::A4,   Note::As4,  Note::B4,
            Note::C5,   Note::Cs5,  Note::D5,   Note::Ds5,  Note::E5,   Note::F5,
            Note::Fs5,  Note::G5,   Note::Gs5,  Note::A5,   Note::As5,  Note::B5,
            Note::C6,   Note::Cs6,  Note::D6,   Note::Ds6,  Note::E6,   Note::F6,
            Note::Fs6,  Note::G6,   Note::Gs6,  Note::A6,   Note::As6,  Note::B6,
            Note::C7,   Note::Cs7,  Note::D7,   Note::Ds7,  Note::E7,   Note::F7,
            Note::Fs7,  Note::G7,   Note::Gs7,  Note::A7,   Note::As7,  Note::B7,
            Note::C8,   Note::Cs8,  Note::D8,   Note::Ds8,  Note::E8,   Note::F8,
            Note::Fs8,  Note::G8,   Note::Gs8,  Note::A8,   Note::As8,  Note::B8,
            Note::C9,   Note::Cs9,  Note::D9,   Note::Ds9,  Note::E9,   Note::F9,
            Note::Fs9,  Note::G9,
        ];
        Some(LUT[midi as usize])
    }

    pub const fn midi(self) -> u8 {
        self as u8
    }

    /// Returns -1 to 9.
    pub const fn octave(self) -> i8 {
        (self as u8 / 12) as i8 - 1
    }

    /// 0-11, where 0 = C.
    pub const fn pitch_class(self) -> u8 {
        self as u8 % 12
    }

    /// Frequency in Hz (A4 = 440 Hz, equal temperament).
    pub fn frequency(self) -> f64 {
        440.0 * libm::pow(2.0, (self as u8 as f64 - 69.0) / 12.0)
    }

    /// Returns `None` if result would be out of MIDI range (0-127).
    pub fn transpose(self, semitones: i8) -> Option<Note> {
        let new_midi = (self as u8 as i16) + (semitones as i16);
        if !(0..=127).contains(&new_midi) {
            None
        } else {
            Note::from_midi(new_midi as u8)
        }
    }
}

impl fmt::Display for Note {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const NAMES: [&str; 12] = [
            "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
        ];
        write!(f, "{}{}", NAMES[self.pitch_class() as usize], self.octave())
    }
}

impl From<Note> for u8 {
    fn from(note: Note) -> u8 {
        note as u8
    }
}

impl TryFrom<u8> for Note {
    type Error = ();

    fn try_from(midi: u8) -> Result<Self, Self::Error> {
        Note::from_midi(midi).ok_or(())
    }
}

/// Flat note aliases.
///
/// These are aliases for the sharp variants, e.g., `Db4` = `Cs4`.
#[allow(non_upper_case_globals)]
pub mod flat {
    use super::Note;

    // Octave -1
    pub const Dbm1: Note = Note::Csm1;
    pub const Ebm1: Note = Note::Dsm1;
    pub const Gbm1: Note = Note::Fsm1;
    pub const Abm1: Note = Note::Gsm1;
    pub const Bbm1: Note = Note::Asm1;

    // Octave 0
    pub const Db0: Note = Note::Cs0;
    pub const Eb0: Note = Note::Ds0;
    pub const Gb0: Note = Note::Fs0;
    pub const Ab0: Note = Note::Gs0;
    pub const Bb0: Note = Note::As0;

    // Octave 1
    pub const Db1: Note = Note::Cs1;
    pub const Eb1: Note = Note::Ds1;
    pub const Gb1: Note = Note::Fs1;
    pub const Ab1: Note = Note::Gs1;
    pub const Bb1: Note = Note::As1;

    // Octave 2
    pub const Db2: Note = Note::Cs2;
    pub const Eb2: Note = Note::Ds2;
    pub const Gb2: Note = Note::Fs2;
    pub const Ab2: Note = Note::Gs2;
    pub const Bb2: Note = Note::As2;

    // Octave 3
    pub const Db3: Note = Note::Cs3;
    pub const Eb3: Note = Note::Ds3;
    pub const Gb3: Note = Note::Fs3;
    pub const Ab3: Note = Note::Gs3;
    pub const Bb3: Note = Note::As3;

    // Octave 4
    pub const Db4: Note = Note::Cs4;
    pub const Eb4: Note = Note::Ds4;
    pub const Gb4: Note = Note::Fs4;
    pub const Ab4: Note = Note::Gs4;
    pub const Bb4: Note = Note::As4;

    // Octave 5
    pub const Db5: Note = Note::Cs5;
    pub const Eb5: Note = Note::Ds5;
    pub const Gb5: Note = Note::Fs5;
    pub const Ab5: Note = Note::Gs5;
    pub const Bb5: Note = Note::As5;

    // Octave 6
    pub const Db6: Note = Note::Cs6;
    pub const Eb6: Note = Note::Ds6;
    pub const Gb6: Note = Note::Fs6;
    pub const Ab6: Note = Note::Gs6;
    pub const Bb6: Note = Note::As6;

    // Octave 7
    pub const Db7: Note = Note::Cs7;
    pub const Eb7: Note = Note::Ds7;
    pub const Gb7: Note = Note::Fs7;
    pub const Ab7: Note = Note::Gs7;
    pub const Bb7: Note = Note::As7;

    // Octave 8
    pub const Db8: Note = Note::Cs8;
    pub const Eb8: Note = Note::Ds8;
    pub const Gb8: Note = Note::Fs8;
    pub const Ab8: Note = Note::Gs8;
    pub const Bb8: Note = Note::As8;

    // Octave 9
    pub const Db9: Note = Note::Cs9;
    pub const Eb9: Note = Note::Ds9;
    pub const Gb9: Note = Note::Fs9;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_octave() {
        assert_eq!(Note::C4.octave(), 4);
        assert_eq!(Note::Cm1.octave(), -1);
        assert_eq!(Note::B0.octave(), 0);
    }

    #[test]
    fn test_pitch_class() {
        assert_eq!(Note::C4.pitch_class(), 0);
        assert_eq!(Note::Cs4.pitch_class(), 1);
        assert_eq!(Note::B4.pitch_class(), 11);
    }

    #[test]
    fn test_frequency() {
        let a4_freq = Note::A4.frequency();
        assert!((a4_freq - 440.0).abs() < 0.01);
    }

    #[test]
    fn test_transpose() {
        assert_eq!(Note::C4.transpose(12), Some(Note::C5));
        assert_eq!(Note::C4.transpose(-12), Some(Note::C3));
        assert_eq!(Note::G9.transpose(1), None); // Would exceed 127
        assert_eq!(Note::Cm1.transpose(-1), None); // Would go below 0
    }

    #[test]
    fn test_from_midi_all_values() {
        // Every valid MIDI note should round-trip
        for n in 0..=127u8 {
            let note = Note::from_midi(n).unwrap();
            assert_eq!(note.midi(), n, "Round-trip failed for MIDI note {n}");
        }
        // 128+ should return None
        assert_eq!(Note::from_midi(128), None);
        assert_eq!(Note::from_midi(255), None);
    }

    #[test]
    fn test_frequency_known_values() {
        // A4 = 440 Hz (exact)
        assert!((Note::A4.frequency() - 440.0).abs() < 0.01);
        // A3 = 220 Hz (one octave down)
        assert!((Note::A3.frequency() - 220.0).abs() < 0.01);
        // A5 = 880 Hz (one octave up)
        assert!((Note::A5.frequency() - 880.0).abs() < 0.01);
        // C4 = 261.63 Hz (middle C)
        assert!((Note::C4.frequency() - 261.63).abs() < 0.1);
        // Frequency must always be positive
        assert!(Note::Cm1.frequency() > 0.0);
        assert!(Note::G9.frequency() > 0.0);
    }

    #[test]
    fn test_transpose_boundary_sweep() {
        // Transpose from lowest note up by max
        assert_eq!(Note::Cm1.transpose(0), Some(Note::Cm1));
        assert_eq!(Note::Cm1.transpose(127), Some(Note::G9));

        // Transpose from highest note down by max
        assert_eq!(Note::G9.transpose(0), Some(Note::G9));
        assert_eq!(Note::G9.transpose(-127), Some(Note::Cm1));
        assert_eq!(Note::G9.transpose(-128), None); // -128 fits in i8

        // Middle note both directions
        assert_eq!(Note::C4.transpose(67), Some(Note::G9)); // 60 + 67 = 127
        assert_eq!(Note::C4.transpose(68), None); // 60 + 68 = 128, out of range
        assert_eq!(Note::C4.transpose(-60), Some(Note::Cm1)); // 60 - 60 = 0
        assert_eq!(Note::C4.transpose(-61), None); // 60 - 61 = -1, out of range
    }

    #[test]
    fn test_display() {
        extern crate alloc;
        use alloc::format;
        assert_eq!(format!("{}", Note::C4), "C4");
        assert_eq!(format!("{}", Note::A4), "A4");
        assert_eq!(format!("{}", Note::Cs4), "C#4");
        assert_eq!(format!("{}", Note::Cm1), "C-1");
        assert_eq!(format!("{}", Note::G9), "G9");
    }
}
