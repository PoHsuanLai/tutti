//! The Flex Data Set Tempo message a clip declares its tempo with (M2-104
//! section 7.5.3; M2-116 section 7.1.1).
//!
//! The message itself is `tutti_ump::Flex`. What is here is the one thing
//! `tutti-ump` cannot say, a tempo in `Bpm` (it depends on no unit crate): the
//! Set Tempo field is 10-nanosecond units per quarter note, and a `Bpm` is the
//! other way up.

use tutti_types::Bpm;
use tutti_ump::{Flex, Message};

/// 10-nanosecond units in one minute: 60 s is 6e9 of them, so a tempo of
/// `bpm` is `6e9 / bpm` units per quarter note.
const TEN_NS_UNITS_PER_MINUTE: f64 = 6_000_000_000.0;

/// The tempo a Set Tempo field declares, `None` for `0` ("no valid tempo").
fn bpm_of(field: u32) -> Option<Bpm> {
    (field != 0)
        .then(|| Bpm::new(TEN_NS_UNITS_PER_MINUTE / f64::from(field)))
        .flatten()
}

/// The tempo of a Set Tempo message, `None` for any other message or a field
/// of `0`.
pub(crate) fn tempo(message: &Message) -> Option<Bpm> {
    match message {
        Message::Flex(_, Flex::SetTempo(field)) => bpm_of(*field),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
