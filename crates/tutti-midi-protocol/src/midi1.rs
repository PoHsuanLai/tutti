//! A message at MIDI 1.0: the narrowing of a MIDI 2.0 voice message, and the
//! MIDI 1.0 messages a [`Message`] stands for.

use tutti_ump::{Message, Midi1Bytes, U7, Ump, Voice1, Voice2};

use crate::{narrow_bend, narrow_value, narrow_velocity};

/// The velocity a Note On narrows to when the shift leaves 0.
const NOTE_ON_FLOOR: U7 = U7::of(1);

/// Why a MIDI 2.0 channel voice message has no single MIDI 1.0 message
/// ([`narrow`]'s refusal): the kind of message, as M2-104 Appendix D.2 groups
/// them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum NoMidi1Form {
    /// A Registered or Assignable Per-Note Controller, or Per-Note Pitch Bend
    /// (D.2.8): MIDI 1.0 has no per-note expression.
    PerNote,
    /// A Registered or Assignable Controller, absolute or relative: MIDI 1.0
    /// sends it as four Control Changes ([`Narrower`](crate::Narrower), which
    /// keeps the select).
    Parameter,
    /// Per-Note Management: MIDI 1.0 has no per-note state to manage.
    PerNoteManagement,
}

impl core::fmt::Display for NoMidi1Form {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::PerNote => "a per-note controller or bend has no MIDI 1.0 message",
            Self::Parameter => "a registered or assignable controller is four MIDI 1.0 messages",
            Self::PerNoteManagement => "per-note management has no MIDI 1.0 message",
        })
    }
}

impl core::error::Error for NoMidi1Form {}

/// `voice` at MIDI 1.0 resolution (M2-115 section 3.4), or the [`NoMidi1Form`]
/// that says why MIDI 1.0 has no single message for it: per-note controllers
/// and bend, RPN and NRPN, per-note management.
///
/// A Note On whose velocity narrows to 0 becomes velocity 1, because a MIDI 1.0
/// Note On at velocity 0 is a Note Off (M2-104 section 7.4.2). A Program Change
/// keeps its program and loses its bank; [`midi1()`](fn@midi1) has the bank. A reserved
/// attribute type is not read: MIDI 1.0 has no attribute.
///
/// # Errors
///
/// [`NoMidi1Form`] for a per-note message, a registered or assignable
/// controller, or per-note management.
pub fn narrow(voice: Voice2) -> Result<Voice1, NoMidi1Form> {
    Ok(match voice {
        Voice2::NoteOn { note, velocity, .. } => Voice1::NoteOn {
            note,
            velocity: narrow_velocity(velocity).max(NOTE_ON_FLOOR),
        },
        Voice2::NoteOff { note, velocity, .. } => Voice1::NoteOff {
            note,
            velocity: narrow_velocity(velocity),
        },
        Voice2::PolyPressure { note, value } => Voice1::PolyPressure {
            note,
            pressure: narrow_value(value),
        },
        Voice2::ControlChange { controller, value } => Voice1::ControlChange {
            controller,
            value: narrow_value(value),
        },
        Voice2::ProgramChange { program, .. } => Voice1::ProgramChange(program),
        Voice2::ChannelPressure(value) => Voice1::ChannelPressure(narrow_value(value)),
        Voice2::PitchBend(bend) => Voice1::PitchBend(narrow_bend(bend)),
        Voice2::RegisteredPerNote { .. }
        | Voice2::AssignablePerNote { .. }
        | Voice2::PerNotePitchBend { .. } => return Err(NoMidi1Form::PerNote),
        Voice2::Registered { .. }
        | Voice2::Assignable { .. }
        | Voice2::RelativeRegistered { .. }
        | Voice2::RelativeAssignable { .. } => return Err(NoMidi1Form::Parameter),
        Voice2::PerNoteManagement { .. } => return Err(NoMidi1Form::PerNoteManagement),
    })
}

/// The MIDI 1.0 messages `message` stands for, in the order to send them:
/// none when it has no MIDI 1.0 form, three for a MIDI 2.0 Program Change with
/// a bank (CC 0 the bank MSB, CC 32 the LSB, then the Program Change; M2-104
/// Appendix D), one otherwise. Channel voice messages narrow as [`narrow`] does.
///
/// Takes a [`Message`] or the [`Ump`] it is (a packet that does not decode has
/// no MIDI 1.0 form).
pub fn midi1(packet: impl Into<Ump>) -> impl Iterator<Item = Midi1Bytes> {
    let none = None;
    let slots = match packet.into().decode() {
        Ok(message) => slots_of(message),
        Err(_) => [none; 3],
    };
    slots.into_iter().flatten()
}

fn slots_of(message: Message) -> [Option<Midi1Bytes>; 3] {
    let none = None;
    match message {
        Message::Midi1(address, voice) => [Some(voice.bytes(address.channel)), none, none],
        Message::System(_, system) => [Some(system.bytes()), none, none],
        Message::Midi2(
            address,
            Voice2::ProgramChange {
                program,
                bank: Some(bank),
            },
        ) => {
            let [msb, lsb] = bank.bytes(address.channel);
            let program = Voice1::ProgramChange(program).bytes(address.channel);
            [Some(msb), Some(lsb), Some(program)]
        }
        Message::Midi2(address, voice) => [
            narrow(voice).ok().map(|voice| voice.bytes(address.channel)),
            none,
            none,
        ],
        Message::Utility(_)
        | Message::Sysex7(..)
        | Message::Data128(..)
        | Message::Flex(..)
        | Message::Stream(_) => [none; 3],
    }
}
