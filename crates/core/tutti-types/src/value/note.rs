//! Notes in twelve-tone equal temperament.
//!
//! A note is a musical fact: C#4 exists whether or not MIDI does. MIDI's
//! contribution is an *encoding* — an integer 0..=127 with a fixed origin —
//! so it appears here as a conversion, not as the type's identity. Naming this
//! `MidiNote` would let one consumer's wire format define what a note is, and
//! would quietly make MIDI's range the limit of what is expressible.
//!
//! It is not: [`Note`] can name C#9 or A-2, neither of which has a MIDI
//! number. That asymmetry is why the conversion out is fallible, and it is the
//! evidence that the two concepts are genuinely different.
//!
//! Existing MIDI code keeps its `u8` — the wire format is honest there. Cross
//! into `Note` where you want a frequency, a name, or transposition.

use super::units::{Cents, Hz, Semitones};

/// One of the twelve pitch classes.
///
/// Replaces the `[&str; 12]` lookup tables that were copied per spelling and
/// indexed by an open-coded `% 12`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum PitchClass {
    C,
    CSharp,
    D,
    DSharp,
    E,
    F,
    FSharp,
    G,
    GSharp,
    A,
    ASharp,
    B,
}

impl PitchClass {
    /// In ascending order from C.
    pub const ALL: [PitchClass; 12] = [
        Self::C,
        Self::CSharp,
        Self::D,
        Self::DSharp,
        Self::E,
        Self::F,
        Self::FSharp,
        Self::G,
        Self::GSharp,
        Self::A,
        Self::ASharp,
        Self::B,
    ];

    /// Semitones above C.
    #[inline]
    pub const fn semitones_above_c(self) -> u8 {
        self as u8
    }

    /// From semitones above C, wrapping every 12.
    #[inline]
    pub const fn from_semitones_above_c(semitones: u8) -> Self {
        Self::ALL[(semitones % 12) as usize]
    }

    /// Sharp spelling: `C`, `C#`, `D`, …
    #[inline]
    pub const fn sharp_name(self) -> &'static str {
        match self {
            Self::C => "C",
            Self::CSharp => "C#",
            Self::D => "D",
            Self::DSharp => "D#",
            Self::E => "E",
            Self::F => "F",
            Self::FSharp => "F#",
            Self::G => "G",
            Self::GSharp => "G#",
            Self::A => "A",
            Self::ASharp => "A#",
            Self::B => "B",
        }
    }

    /// Flat spelling: `C`, `Db`, `D`, … The same twelve pitches, respelled.
    #[inline]
    pub const fn flat_name(self) -> &'static str {
        match self {
            Self::C => "C",
            Self::CSharp => "Db",
            Self::D => "D",
            Self::DSharp => "Eb",
            Self::E => "E",
            Self::F => "F",
            Self::FSharp => "Gb",
            Self::G => "G",
            Self::GSharp => "Ab",
            Self::A => "A",
            Self::ASharp => "Bb",
            Self::B => "B",
        }
    }

    /// Whether this class is spelled with an accidental.
    #[inline]
    pub const fn is_accidental(self) -> bool {
        matches!(
            self,
            Self::CSharp | Self::DSharp | Self::FSharp | Self::GSharp | Self::ASharp
        )
    }
}

/// A note: a pitch class in a numbered octave.
///
/// Octaves use scientific pitch notation, where middle C is C4 and A4 is
/// 440 Hz. MIDI's own numbering is a *different* convention and lives in the
/// conversion rather than here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Note {
    class: PitchClass,
    octave: i8,
}

/// A [`Note`] outside MIDI's 0..=127 range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotOnMidiScale(pub Note);

impl core::fmt::Display for NotOnMidiScale {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} has no MIDI note number", self.0)
    }
}

impl core::error::Error for NotOnMidiScale {}

/// A MIDI note number above 127.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoteNumberOutOfRange(pub u8);

impl core::fmt::Display for NoteNumberOutOfRange {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "MIDI note number {} is above 127", self.0)
    }
}

impl core::error::Error for NoteNumberOutOfRange {}

impl Note {
    /// Concert A — 440 Hz by definition.
    pub const A4: Note = Note {
        class: PitchClass::A,
        octave: 4,
    };

    /// Middle C.
    pub const C4: Note = Note {
        class: PitchClass::C,
        octave: 4,
    };

    #[inline]
    pub const fn new(class: PitchClass, octave: i8) -> Self {
        Self { class, octave }
    }

    #[inline]
    pub const fn class(self) -> PitchClass {
        self.class
    }

    #[inline]
    pub const fn octave(self) -> i8 {
        self.octave
    }

    /// Semitones from C0, which may be negative for sub-zero octaves.
    #[inline]
    pub const fn semitones_from_c0(self) -> i32 {
        self.octave as i32 * 12 + self.class.semitones_above_c() as i32
    }

    /// From semitones above C0.
    #[inline]
    pub fn from_semitones_from_c0(semitones: i32) -> Self {
        // `rem_euclid`, not `%`: a negative index must wrap into the octave
        // below rather than reflecting to a negative pitch class.
        Self {
            class: PitchClass::from_semitones_above_c(semitones.rem_euclid(12) as u8),
            octave: semitones.div_euclid(12) as i8,
        }
    }

    /// Interval from `self` up to `other`.
    #[inline]
    pub fn interval_to(self, other: Note) -> Semitones {
        Semitones((other.semitones_from_c0() - self.semitones_from_c0()) as f32)
    }

    /// Move by a whole number of semitones. Fractional input is truncated —
    /// a note is a discrete pitch, so use [`frequency`](Self::frequency) and
    /// detune from there when you want the space between two notes.
    #[inline]
    pub fn transpose(self, by: Semitones) -> Self {
        Self::from_semitones_from_c0(self.semitones_from_c0() + by.get() as i32)
    }

    /// Frequency in A440 twelve-tone equal temperament.
    ///
    /// This is the plain default. Alternative temperaments — just intonation,
    /// Pythagorean, meantone — are a tuning table's job, not a note's; see
    /// `tutti_synth::tuning`.
    #[inline]
    pub fn frequency(self) -> Hz {
        let steps = (self.semitones_from_c0() - Self::A4.semitones_from_c0()) as f32;
        Self::A4_HZ * Semitones(steps).to_pitch_ratio()
    }

    /// The nearest note to `freq`, and how far off it is.
    ///
    /// The offset is signed and lands in −50..=+50 cents: past that, a
    /// different note is nearer.
    pub fn nearest_to(freq: Hz) -> Option<(Self, Cents)> {
        if freq.get() <= 0.0 {
            return None;
        }
        // The named inverse of `frequency`'s `Semitones::to_pitch_ratio`, which
        // is what this line used to spell out as `12.0 * ratio.log2()`. Both
        // directions now go through the converter, and the guard above is the
        // same one `from_pitch_ratio` applies — kept because `None` is a
        // better answer here than a unison.
        let steps = Semitones::from_pitch_ratio(freq.get() / Self::A4_HZ.get());
        let nearest = steps.get().round();
        let note = Self::from_semitones_from_c0(Self::A4.semitones_from_c0() + nearest as i32);
        Some((note, Semitones(steps.get() - nearest).to_cents()))
    }

    /// Sharp spelling with octave: `A4`, `C#5`.
    pub fn sharp_name(self) -> String {
        format!("{}{}", self.class.sharp_name(), self.octave)
    }

    /// Flat spelling with octave: `A4`, `Db5`.
    pub fn flat_name(self) -> String {
        format!("{}{}", self.class.flat_name(), self.octave)
    }

    /// Concert A's frequency. The one tuning constant this module carries.
    const A4_HZ: Hz = Hz(440.0);

    /// MIDI note 0 is C-1 in scientific pitch notation. This offset is a MIDI
    /// convention, so it lives with the conversion rather than on the note.
    const MIDI_C_OCTAVE: i32 = -1;
}

impl core::fmt::Display for Note {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}{}", self.class.sharp_name(), self.octave)
    }
}

// ── MIDI encoding ───────────────────────────────────────────────────────────
//
// One encoding of a note, not the note itself. Both directions are fallible
// because the two ranges genuinely differ: MIDI cannot name C#9, and a `u8`
// can hold 200.

impl TryFrom<u8> for Note {
    type Error = NoteNumberOutOfRange;

    /// MIDI note number → note. `69` is A4.
    fn try_from(number: u8) -> Result<Self, Self::Error> {
        if number > 127 {
            return Err(NoteNumberOutOfRange(number));
        }
        Ok(Self::from_semitones_from_c0(
            number as i32 + Self::MIDI_C_OCTAVE * 12,
        ))
    }
}

impl TryFrom<Note> for u8 {
    type Error = NotOnMidiScale;

    /// Note → MIDI note number, if the note is on MIDI's scale at all.
    fn try_from(note: Note) -> Result<Self, Self::Error> {
        let number = note.semitones_from_c0() - Note::MIDI_C_OCTAVE * 12;
        u8::try_from(number)
            .ok()
            .filter(|n| *n <= 127)
            .ok_or(NotOnMidiScale(note))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a4_is_440_hz_by_definition() {
        assert!((Note::A4.frequency().get() - 440.0).abs() < 1e-3);
        assert_eq!(Note::A4.sharp_name(), "A4");
    }

    #[test]
    fn an_octave_doubles_the_frequency() {
        let a5 = Note::A4.transpose(Semitones::OCTAVE);
        assert_eq!(a5, Note::new(PitchClass::A, 5));
        assert!((a5.frequency().get() - 880.0).abs() < 1e-2);

        let a3 = Note::A4.transpose(Semitones(-12.0));
        assert!((a3.frequency().get() - 220.0).abs() < 1e-2);
    }

    #[test]
    fn the_two_spellings_name_the_same_pitch() {
        let cs4 = Note::new(PitchClass::CSharp, 4);
        assert_eq!(cs4.sharp_name(), "C#4");
        assert_eq!(cs4.flat_name(), "Db4");
        assert!(cs4.class().is_accidental());
        assert!(!Note::C4.class().is_accidental());
    }

    #[test]
    fn nearest_note_reports_a_signed_offset() {
        let (note, cents) = Note::nearest_to(Hz(440.0)).unwrap();
        assert_eq!(note, Note::A4);
        assert!(cents.get().abs() < 1e-3);

        // Slightly sharp of A4.
        let (note, cents) = Note::nearest_to(Hz(445.0)).unwrap();
        assert_eq!(note, Note::A4);
        assert!(cents.get() > 0.0 && cents.get() < 50.0);

        // Slightly flat.
        let (note, cents) = Note::nearest_to(Hz(435.0)).unwrap();
        assert_eq!(note, Note::A4);
        assert!(cents.get() < 0.0 && cents.get() > -50.0);

        assert!(Note::nearest_to(Hz(0.0)).is_none());
        assert!(Note::nearest_to(Hz(-1.0)).is_none());
    }

    #[test]
    fn every_note_round_trips_through_its_own_frequency() {
        for semitones in -12..120 {
            let note = Note::from_semitones_from_c0(semitones);
            let (back, cents) = Note::nearest_to(note.frequency()).unwrap();
            assert_eq!(back, note, "{note} did not round trip");
            assert!(cents.get().abs() < 0.01, "{note} drifted {cents}");
        }
    }

    /// Sub-zero octaves must wrap down, not reflect — `%` would give a
    /// negative pitch class.
    #[test]
    fn negative_octaves_wrap_downward() {
        let b_minus_1 = Note::from_semitones_from_c0(-1);
        assert_eq!(b_minus_1.class(), PitchClass::B);
        assert_eq!(b_minus_1.octave(), -1);

        let c_minus_1 = Note::from_semitones_from_c0(-12);
        assert_eq!(c_minus_1.class(), PitchClass::C);
        assert_eq!(c_minus_1.octave(), -1);
    }

    #[test]
    fn intervals_are_signed_and_symmetric() {
        let c4 = Note::C4;
        let g4 = Note::new(PitchClass::G, 4);

        assert_eq!(c4.interval_to(g4), Semitones(7.0));
        assert_eq!(g4.interval_to(c4), Semitones(-7.0));
        assert_eq!(c4.interval_to(c4), Semitones(0.0));
    }

    // ── MIDI encoding ───────────────────────────────────────────────────

    #[test]
    fn midi_note_69_is_a4() {
        assert_eq!(Note::try_from(69u8).unwrap(), Note::A4);
        assert_eq!(u8::try_from(Note::A4).unwrap(), 69);

        // MIDI 0 is C-1, and 60 is middle C.
        assert_eq!(Note::try_from(0u8).unwrap(), Note::new(PitchClass::C, -1));
        assert_eq!(Note::try_from(60u8).unwrap(), Note::C4);
    }

    #[test]
    fn every_midi_note_round_trips() {
        for number in 0u8..=127 {
            let note = Note::try_from(number).unwrap();
            assert_eq!(u8::try_from(note).unwrap(), number);
        }
    }

    /// The asymmetry that makes this a conversion rather than an identity:
    /// notes exist that MIDI cannot name, and `u8`s exist that are not notes.
    #[test]
    fn the_two_ranges_do_not_coincide() {
        assert_eq!(Note::try_from(128u8), Err(NoteNumberOutOfRange(128)));
        assert_eq!(Note::try_from(255u8), Err(NoteNumberOutOfRange(255)));

        // Above MIDI's ceiling (G9 is 127).
        let too_high = Note::new(PitchClass::CSharp, 10);
        assert_eq!(u8::try_from(too_high), Err(NotOnMidiScale(too_high)));

        // Below its floor (C-1 is 0).
        let too_low = Note::new(PitchClass::A, -2);
        assert_eq!(u8::try_from(too_low), Err(NotOnMidiScale(too_low)));
    }

    #[test]
    fn pitch_classes_enumerate_in_order() {
        assert_eq!(PitchClass::ALL.len(), 12);
        for (i, class) in PitchClass::ALL.iter().enumerate() {
            assert_eq!(class.semitones_above_c(), i as u8);
            assert_eq!(PitchClass::from_semitones_above_c(i as u8), *class);
        }
        // Wraps at the octave.
        assert_eq!(PitchClass::from_semitones_above_c(12), PitchClass::C);
        assert_eq!(PitchClass::from_semitones_above_c(13), PitchClass::CSharp);
    }
}
