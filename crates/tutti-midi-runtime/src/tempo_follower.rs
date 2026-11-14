//! [`TempoFollower`]: what a transport that follows an outside tempo applies,
//! as a pure value, scoped to Flex.
//!
//! It reads decoded UMPs and says **what** to apply, a tempo ([`Bpm`]) or a
//! meter ([`TimeSignature`]); it does not say where. It holds no clock, no
//! tempo map and no OS handle, and allocates nothing: whoever owns the
//! transport places the change (the session, in `tutti-session`).
//!
//! The source is an enum, [`TempoSource`], with [`Flex`](TempoSource::Flex)
//! first. MIDI Beat Clock and MTC are followed by [`ClockFollower`](crate::ClockFollower).
//!
//! **Flex addressing** (M2-104 §7.5, Table 10). Set Tempo (§7.5.3) and Set Time
//! Signature (§7.5.4) are Group messages: Format Complete, Address 1, and a Group
//! is a cable of its own (§2.1.2, Appendix B.1). The follower takes those of the
//! followed Group only. A Set Tempo addressed to a Channel is no Set Tempo (the
//! decoder in `tutti-ump` refuses it) and is ignored here too.

use tutti_midi_protocol::{flex_tempo, flex_time_signature};
use tutti_types::{Bpm, TimeSignature};
use tutti_ump::{Flex, Message, MidiGroup, Ump};

/// Where a followed tempo comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TempoSource {
    /// The Flex Data Set Tempo and Set Time Signature messages of one Group
    /// (M2-104 §7.5.3, §7.5.4).
    Flex(MidiGroup),
}

/// What a message asks the transport to take.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum Followed {
    /// A tempo: Set Tempo, `6e9 / field` beats per minute ([`flex_tempo`]).
    Tempo(Bpm),
    /// A meter: Set Time Signature ([`flex_time_signature`]). Only where the
    /// follower was built to follow the meter.
    Meter(TimeSignature),
}

/// Reads a source's messages for the tempo and meter they declare.
///
/// ```
/// use tutti_midi_runtime::{Followed, TempoFollower, TempoSource};
/// use tutti_types::Bpm;
/// use tutti_ump::{Flex, Message, MidiGroup, Ump};
///
/// let follower = TempoFollower::new(TempoSource::Flex(MidiGroup::FIRST), false);
/// // 0.5 s per quarter note is 120 bpm.
/// let set_tempo = Ump::from(Message::Flex(MidiGroup::FIRST, Flex::SetTempo(50_000_000)));
/// assert_eq!(follower.step(set_tempo), Some(Followed::Tempo(Bpm::of(120.0))));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TempoFollower {
    source: TempoSource,
    meter: bool,
}

impl TempoFollower {
    /// A follower of `source`; `meter` says whether it also reads the time
    /// signature (a Set Time Signature is ignored otherwise).
    #[must_use]
    pub const fn new(source: TempoSource, meter: bool) -> Self {
        Self { source, meter }
    }

    /// Where it reads from.
    #[must_use]
    pub const fn source(&self) -> TempoSource {
        self.source
    }

    /// What `ump` asks the transport to take, if anything: a Set Tempo or (when
    /// the meter is followed) a Set Time Signature of the followed Group, whose
    /// value the project's units can hold. `None` for any other message, for one
    /// of another Group, and for what [`flex_tempo`] and [`flex_time_signature`]
    /// refuse (a tempo field of `0`, a numerator of `0` or above 64, a
    /// non-standard or too short denominator).
    #[must_use]
    pub fn step(&self, ump: impl Into<Ump>) -> Option<Followed> {
        let TempoSource::Flex(group) = self.source;
        let Ok(Message::Flex(from, flex)) = ump.into().decode() else {
            return None;
        };
        if from != group {
            return None;
        }
        match flex {
            Flex::SetTempo(field) => flex_tempo(field).map(Followed::Tempo),
            Flex::SetTimeSignature(wire) if self.meter => {
                flex_time_signature(wire).map(Followed::Meter)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests;
