//! The Flex Data values the transport follows, as the project's unit types:
//! Set Tempo (M2-104 section 7.5.3) as a [`Bpm`] and Set Time Signature
//! (section 7.5.4) as a [`tutti_types::TimeSignature`]. [`tutti_ump::Flex`]
//! holds the wire's integers and depends on no unit crate; this is the one
//! reading of them.

use tutti_types::{BeatsPerBar, Bpm, NoteValue};
use tutti_ump::TimeSignature as FlexTimeSignature;

/// 10-nanosecond units in one minute: 60 s is 6e9 of them, so a tempo of
/// `bpm` is `6e9 / bpm` units per quarter note.
const TEN_NS_UNITS_PER_MINUTE: f64 = 6_000_000_000.0;

/// The tempo a Set Tempo field declares: the field is 10 ns units per quarter
/// note (section 7.5.3), so `6e9 / field` beats per minute. `None` for `0`, no
/// valid tempo. The slowest the field holds is 1.39698 bpm.
///
/// ```
/// use tutti_midi_protocol::flex_tempo;
/// use tutti_types::Bpm;
///
/// // 0.5 s per quarter note is 120 bpm.
/// assert_eq!(flex_tempo(50_000_000), Bpm::new(120.0));
/// assert_eq!(flex_tempo(0), None);
/// ```
#[must_use]
pub fn flex_tempo(field: u32) -> Option<Bpm> {
    (field != 0)
        .then(|| Bpm::new(TEN_NS_UNITS_PER_MINUTE / f64::from(field)))
        .flatten()
}

/// The meter a Set Time Signature message declares: the numerator field is the
/// beats per bar, the denominator field a negative power of 2 (2 is a quarter
/// note, 3 an eighth). `None` for what the project's meter cannot hold: a
/// numerator of 0 or above 64 ([`BeatsPerBar`]'s ceiling),
/// a denominator field of 0 (non-standard, section 7.5.4) or
/// one that names a note shorter than a 64th note ([`NoteValue`]'s ceiling).
///
/// The message's third field, the number of 1/32 notes in 24 MIDI Clocks, is
/// how the sender relates a quarter note to its notation and does not change
/// the meter; it is not read here.
///
/// ```
/// use tutti_midi_protocol::flex_time_signature;
/// use tutti_types::TimeSignature;
/// use tutti_ump::TimeSignature as Wire;
///
/// let seven_eight = Wire { numerator: 7, denominator: 3, thirty_seconds: 8 };
/// assert_eq!(flex_time_signature(seven_eight), Some(TimeSignature::of(7, 8)));
/// ```
#[must_use]
pub fn flex_time_signature(wire: FlexTimeSignature) -> Option<tutti_types::TimeSignature> {
    let beats = BeatsPerBar::new(u32::from(wire.numerator))?;
    // The field is an exponent, so a shift past the word is no note value.
    let value = 1u32
        .checked_shl(u32::from(wire.denominator))
        .filter(|_| wire.denominator != 0)?;
    Some(tutti_types::TimeSignature::new(
        beats,
        NoteValue::new(value)?,
    ))
}
