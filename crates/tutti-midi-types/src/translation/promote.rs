//! **MIDI 1.0 → MIDI 2.0 Default Translation** (M2-104 Appendix D.3), stateless
//! and per-message — the one seam that hides the MIDI 1.0 ↔ 2.0 differences so
//! every consumer matches a single vocabulary: MIDI 2.0 Channel Voice via `midi2`.
//! (The multi-message parts of Default Translation, RPN/NRPN runs and Bank
//! Select, are `tutti_midi_protocol::Translator`'s.)
//!
//! [`normalize`] takes any [`Ump`] and returns a **MIDI 2.0 Channel Voice**
//! packet (still a [`Ump`], i.e. an owned `[u32; 4]`), applying:
//!   * **velocity-0 NoteOn ⇒ NoteOff** (the MIDI 1.0 historical quirk), and
//!   * **Channel Voice 1 ⇒ Channel Voice 2 promotion** — 7/14-bit fields widened
//!     to 16/32-bit by `tutti_midi_protocol::widen`'s spec Min-Center-Max.
//!
//! Consumers then decode with the same `midi2::UmpMessage::try_from(ump.words())`
//! they would use anyway — but on a stream that is guaranteed single-protocol, so
//! their `match` never needs a Channel Voice 1 arm. Non-channel-voice messages
//! (system, utility, SysEx, and CV2 messages with no MIDI-1 analogue) pass through
//! unchanged.

use midi2::channel_voice2::ChannelVoice2;
use midi2::{Channeled, Grouped, UmpMessage};
use tutti_midi_protocol::widen;
use tutti_ump::{
    Address, Attribute, Message, MidiChannel, MidiGroup, NoteNumber, Ump, Voice1, Voice2, decode,
};

/// Normalize `ump` to a MIDI 2.0 Channel Voice [`Ump`]. See module docs.
///
/// Returns the packet unchanged when it is already MIDI-2 Channel Voice (aside
/// from the velocity-0 fold) or when it carries no channel-voice payload.
/// Preserves `group`.
pub fn normalize(ump: &Ump) -> Ump {
    let Ok(msg) = UmpMessage::try_from(ump.words()) else {
        return *ump;
    };
    let out = match msg {
        UmpMessage::ChannelVoice2(cv2) => fold_note_off_cv2(cv2),
        UmpMessage::ChannelVoice1(_) => promote_cv1(ump),
        _ => None,
    };
    out.unwrap_or(*ump)
}

/// One MIDI 2.0 Channel Voice message as a packet.
fn voice(group: MidiGroup, channel: MidiChannel, v: Voice2) -> Ump {
    Message::Midi2(Address { group, channel }, v).into()
}

/// A note number from the wire, 7-bit by construction (`& 0x7F` keeps the old
/// constructors' masking).
fn note(n: u8) -> NoteNumber {
    NoteNumber::new(n & 0x7F).unwrap_or_default()
}

fn note_off(group: MidiGroup, channel: MidiChannel, n: u8, velocity: u16) -> Ump {
    voice(
        group,
        channel,
        Voice2::NoteOff {
            note: note(n),
            velocity,
            attribute: Attribute::None,
        },
    )
}

/// A velocity-0 CV2 NoteOn becomes a NoteOff; every other CV2 message is left
/// as-is (returned `None` so the caller keeps the original packet verbatim).
fn fold_note_off_cv2(cv2: ChannelVoice2<&[u32]>) -> Option<Ump> {
    match cv2 {
        ChannelVoice2::NoteOn(m) if m.velocity() == 0 => {
            let g = MidiGroup::masked(u8::from(m.group()));
            Some(note_off(
                g,
                MidiChannel::masked(u8::from(m.channel())),
                u8::from(m.note_number()),
                0,
            ))
        }
        _ => None,
    }
}

/// Promote a MIDI 1.0 Channel Voice message to the MIDI 2.0 equivalent
/// (`tutti_midi_protocol::widen`). Velocity-0 NoteOn folds to NoteOff.
fn promote_cv1(ump: &Ump) -> Option<Ump> {
    let Ok(Message::Midi1(address, voice)) = decode(*ump) else {
        return None;
    };
    let wide = match voice {
        Voice1::NoteOn { note, velocity } if velocity.get() == 0 => Voice2::NoteOff {
            note,
            velocity: 0,
            attribute: Attribute::None,
        },
        other => widen(other),
    };
    Some(Message::Midi2(address, wide).into())
}

#[cfg(test)]
mod tests;
