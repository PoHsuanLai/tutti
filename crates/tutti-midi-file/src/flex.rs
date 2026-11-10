//! The two Flex Data messages a clip declares its musical context with: Set
//! Tempo and Set Time Signature (M2-104 sections 7.5.3 and 7.5.4; M2-116
//! sections 7.1.1 and 7.1.2).
//!
//! The messages themselves are `tutti_ump::Flex`. What is here is the one
//! thing `tutti-ump` cannot say, a tempo in `Bpm` (it depends on no unit
//! crate): the Set Tempo field is 10-nanosecond units per quarter note, and a
//! `Bpm` is the other way up.

use tutti_types::Bpm;
use tutti_ump::{Flex, Message, MidiGroup, TimeSignature, Ump, decode};

/// 10-nanosecond units in one minute: 60 s is 6e9 of them, so a tempo of
/// `bpm` is `6e9 / bpm` units per quarter note.
const TEN_NS_UNITS_PER_MINUTE: f64 = 6_000_000_000.0;

/// A Set Tempo field for `bpm`. It saturates at both ends: at `u32::MAX` (the
/// field's slowest tempo, 1.39698 bpm, M2-104 section 7.5.3) and at 1; `0`,
/// "no valid tempo" on the wire, is never written for a tempo.
fn tempo_field(bpm: Bpm) -> u32 {
    ((TEN_NS_UNITS_PER_MINUTE / bpm.get()) as u32).max(1)
}

/// The tempo a Set Tempo field declares, `None` for `0`.
fn bpm_of(field: u32) -> Option<Bpm> {
    (field != 0)
        .then(|| Bpm::new(TEN_NS_UNITS_PER_MINUTE / f64::from(field)))
        .flatten()
}

/// A Set Tempo message for `bpm` on `group`.
pub(crate) fn set_tempo(group: MidiGroup, bpm: Bpm) -> Ump {
    Message::Flex(group, Flex::SetTempo(tempo_field(bpm))).into()
}

/// A Set Time Signature message on `group`, `numerator` and `denominator` as
/// the clip stores them, and the standard 8 thirty-second notes to the
/// quarter.
pub(crate) fn set_time_signature(group: MidiGroup, numerator: u8, denominator: u8) -> Ump {
    Message::Flex(
        group,
        Flex::SetTimeSignature(TimeSignature {
            numerator,
            denominator,
            thirty_seconds: 8,
        }),
    )
    .into()
}

/// The tempo of a Set Tempo message, `None` for any other packet or a field of
/// `0`.
pub(crate) fn tempo(ump: &Ump) -> Option<Bpm> {
    match decode(*ump) {
        Ok(Message::Flex(_, Flex::SetTempo(field))) => bpm_of(field),
        _ => None,
    }
}

/// `(numerator, denominator)` of a Set Time Signature message, `None` for any
/// other packet.
pub(crate) fn time_signature(ump: &Ump) -> Option<(u8, u8)> {
    match decode(*ump) {
        Ok(Message::Flex(_, Flex::SetTimeSignature(t))) => Some((t.numerator, t.denominator)),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
