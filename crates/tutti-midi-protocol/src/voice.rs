//! The stateless `Voice1` to `Voice2` widening, and reading a channel voice
//! message through it.

use crate::scale;
use tutti_ump::{Address, Attribute, Message, Ump, Voice1, Voice2, decode};

/// The release velocity a MIDI 1.0 Note On at velocity 0 becomes (M2-104
/// Appendix D.3.1).
const NOTE_ON_ZERO_RELEASE: u16 = 0x8000;

/// The MIDI 2.0 message that means what `voice` means (M2-104 Appendix D.3,
/// M2-115 section 3). The caller keeps the [`Address`]
/// unchanged.
///
/// Transitional for a consumer: see the crate docs. Control Changes that
/// would need RPN or Bank Select assembly widen by value as plain Control
/// Changes, and a Program Change has no bank.
#[inline]
#[must_use]
pub fn widen(voice: Voice1) -> Voice2 {
    match voice {
        Voice1::NoteOn { note, velocity } if velocity.get() == 0 => Voice2::NoteOff {
            note,
            velocity: NOTE_ON_ZERO_RELEASE,
            attribute: Attribute::None,
        },
        Voice1::NoteOn { note, velocity } => Voice2::NoteOn {
            note,
            velocity: scale::velocity(velocity.get()),
            attribute: Attribute::None,
        },
        Voice1::NoteOff { note, velocity } => Voice2::NoteOff {
            note,
            velocity: scale::velocity(velocity.get()),
            attribute: Attribute::None,
        },
        Voice1::PolyPressure { note, pressure } => Voice2::PolyPressure {
            note,
            value: scale::value(pressure.get()),
        },
        Voice1::ControlChange { controller, value } => Voice2::ControlChange {
            controller,
            value: scale::value(value.get()),
        },
        Voice1::ProgramChange(program) => Voice2::ProgramChange {
            program,
            bank: None,
        },
        Voice1::ChannelPressure(pressure) => Voice2::ChannelPressure(scale::value(pressure.get())),
        Voice1::PitchBend(bend) => Voice2::PitchBend(scale::bend(bend.get())),
    }
}

/// `message` as MIDI 2.0 when it is a MIDI 1.0 channel voice message
/// ([`widen`], address kept); every other message as it came.
///
/// Stateless, like `widen`: a MIDI 1.0 RPN run widens as plain Control
/// Changes. An edge that can see a whole stream runs a
/// [`Translator`](crate::Translator) instead.
#[inline]
#[must_use]
pub fn widen_message(message: Message) -> Message {
    match message {
        Message::Midi1(address, voice) => Message::Midi2(address, widen(voice)),
        other => other,
    }
}

/// The channel voice message `ump` carries, as the MIDI 2.0 message it means
/// (M2-104 Appendix D.3: a MIDI 1.0 one is widened, so its Note On at velocity
/// 0 is a Note Off, while a MIDI 2.0 Note On at velocity 0 stays a Note On), or
/// `None` for any other packet, one [`decode`] cannot frame included.
///
/// What an instrument reads: one match on [`Voice2`], whichever protocol the
/// packet was sent in.
#[inline]
#[must_use]
pub fn channel_voice(ump: Ump) -> Option<(Address, Voice2)> {
    match decode(ump).ok().map(widen_message)? {
        Message::Midi2(address, voice) => Some((address, voice)),
        _ => None,
    }
}
