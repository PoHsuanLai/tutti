//! The stateless `Voice1` to `Voice2` widening, and reading a channel voice
//! message through it.

use crate::scale;
use tutti_ump::{
    Address, Attribute, BendValue, ControlValue, Message, MidiGroup, NoteVelocity, U7, U14, Ump,
    Voice1, Voice2, parse_exact,
};

/// The release velocity a MIDI 1.0 Note On at velocity 0 becomes (M2-104
/// Appendix D.3.1).
const NOTE_ON_ZERO_RELEASE: NoteVelocity = NoteVelocity::new(0x8000);

/// A 7-bit velocity as the 16-bit field of a Note On or Note Off, scaled
/// Min-Center-Max (M2-115 section 3.4): 0 stays 0, 64 is `0x8000` and 127 is
/// `0xFFFF`. [`narrow_velocity`](crate::narrow_velocity) is its right shift
/// back.
///
/// This is the named way from a MIDI 1.0 velocity to a MIDI 2.0 one:
/// `NoteVelocity::new(100)` is a velocity of 100 out of 65535, not a MIDI 1.0
/// 100.
///
/// # Examples
///
/// ```
/// use tutti_midi_protocol::widen_velocity;
/// use tutti_ump::{Message, MidiChannel, NoteNumber, U7, Voice2};
///
/// let velocity = widen_velocity(U7::of(100));
/// assert_eq!(velocity.get(), 51492);
/// let on = Message::Midi2(MidiChannel::FIRST.into(), Voice2::note_on(NoteNumber::of(60), velocity));
/// # let _ = on;
/// ```
#[inline]
#[must_use]
pub const fn widen_velocity(velocity: U7) -> NoteVelocity {
    NoteVelocity::new(scale::upscale(velocity.get() as u32, 7, 16) as u16)
}

/// A 7-bit controller or pressure value as the 32-bit word of a MIDI 2.0
/// Control Change or pressure message, scaled Min-Center-Max (M2-115 section
/// 3): 0 stays 0, 64 is [`ControlValue::CENTER`] and 127 is
/// [`ControlValue::MAX`]. [`narrow_value`](crate::narrow_value) is its right
/// shift back.
///
/// # Examples
///
/// ```
/// use tutti_midi_protocol::widen_value;
/// use tutti_ump::{CCNumber, Message, MidiChannel, U7, Voice2};
///
/// let cc = Message::Midi2(
///     MidiChannel::FIRST.into(),
///     Voice2::ControlChange { controller: CCNumber::VOLUME, value: widen_value(U7::of(127)) },
/// );
/// # let _ = cc;
/// ```
#[inline]
#[must_use]
pub const fn widen_value(value: U7) -> ControlValue {
    ControlValue::new(scale::value(value.get()))
}

/// A 14-bit pitch bend as the 32-bit word of a MIDI 2.0 Pitch Bend, scaled
/// Min-Center-Max (M2-115 section 3): the 14-bit centre `0x2000` is
/// [`BendValue::CENTER`]. [`narrow_bend`](crate::narrow_bend) is its right
/// shift back.
///
/// # Examples
///
/// ```
/// use tutti_midi_protocol::widen_bend;
/// use tutti_ump::{BendValue, U14};
///
/// assert_eq!(widen_bend(U14::of(0x2000)), BendValue::CENTER);
/// ```
#[inline]
#[must_use]
pub const fn widen_bend(bend: U14) -> BendValue {
    BendValue::new(scale::bend(bend.get()))
}

/// The MIDI 2.0 message that means what `voice` means (M2-104 Appendix D.3,
/// M2-115 section 3). The caller keeps the [`Address`] unchanged.
///
/// Control Changes that would need RPN or Bank Select assembly widen by value
/// as plain Control Changes, and a Program Change has no bank.
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
            velocity: widen_velocity(velocity),
            attribute: Attribute::None,
        },
        Voice1::NoteOff { note, velocity } => Voice2::NoteOff {
            note,
            velocity: widen_velocity(velocity),
            attribute: Attribute::None,
        },
        Voice1::PolyPressure { note, pressure } => Voice2::PolyPressure {
            note,
            value: widen_value(pressure),
        },
        Voice1::ControlChange { controller, value } => Voice2::ControlChange {
            controller,
            value: widen_value(value),
        },
        Voice1::ProgramChange(program) => Voice2::ProgramChange {
            program,
            bank: None,
        },
        Voice1::ChannelPressure(pressure) => Voice2::ChannelPressure(widen_value(pressure)),
        Voice1::PitchBend(bend) => Voice2::PitchBend(widen_bend(bend)),
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

/// The packet for the MIDI 1.0 message at the start of `bytes` (a plugin's or
/// a file's wire bytes, padding after the message ignored), in `group`: read by
/// [`parse_exact`] and [`widen_message`]d, so a channel voice message
/// is MIDI 2.0 and a System one is as it is.
///
/// `None` when `parse_exact` reads none: a SysEx or undefined status,
/// a message cut short, or a data byte with its top bit set.
#[inline]
#[must_use]
pub fn widen_bytes(group: MidiGroup, bytes: &[u8]) -> Option<Ump> {
    parse_exact(group, bytes).map(|message| Ump::from(widen_message(message)))
}

/// The channel voice message `ump` carries, as the MIDI 2.0 message it means
/// (M2-104 Appendix D.3: a MIDI 1.0 one is widened, so its Note On at velocity
/// 0 is a Note Off, while a MIDI 2.0 Note On at velocity 0 stays a Note On), or
/// `None` for any other packet, one [`Ump::decode`] cannot frame included.
///
/// What an instrument reads: one match on [`Voice2`], whichever protocol the
/// packet was sent in.
#[inline]
#[must_use]
pub fn channel_voice(packet: impl Into<Ump>) -> Option<(Address, Voice2)> {
    match packet.into().decode().ok().map(widen_message)? {
        Message::Midi2(address, voice) => Some((address, voice)),
        _ => None,
    }
}
