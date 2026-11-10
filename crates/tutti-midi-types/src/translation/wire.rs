//! MIDI 1.0 wire format ↔ UMP conversion: parse 1-3 byte MIDI-1 messages into a
//! [`Ump`] and emit the MIDI-1 wire form back out.

use midi2::prelude::*;
use tutti_ump::{Midi1Parser, MidiGroup, Ump};

/// The bytes were not a parseable MIDI 1.0 channel-voice or system message
/// (malformed, or a SysEx — use [`sysex7_fragments`](crate::ump::sysex7_fragments) for that).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MidiParseError;

impl core::fmt::Display for MidiParseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("not a parseable MIDI 1.0 message")
    }
}

impl std::error::Error for MidiParseError {}

/// Parses MIDI 1.0 wire bytes like [`from_midi1_bytes`], reporting a refusal as
/// a [`MidiParseError`] (the `Result` spelling, for a caller that propagates).
///
/// # Errors
///
/// [`MidiParseError`] where [`from_midi1_bytes`] returns `None`.
#[inline]
pub fn parse_midi1_bytes(bytes: &[u8]) -> Result<Ump, MidiParseError> {
    from_midi1_bytes(bytes).ok_or(MidiParseError)
}

/// Parses one MIDI 1.0 channel-voice or system message at the start of
/// `bytes` into a UMP of type 0x2 (Channel Voice 1) or 0x1 (System), keeping
/// the 7-bit values. Bytes after the message are ignored, as the padding of a
/// fixed three-byte field is. Returns `None` on malformed input, an undefined
/// status or SysEx (use [`sysex7_fragments`](crate::ump::sysex7_fragments) for
/// that). Never allocates.
///
/// Everything lands on [`MidiGroup::FIRST`], and that is not a placeholder:
/// MIDI 1.0 has no group field at all. A DIN stream *is* one cable, so the
/// group a parsed 1.0 message belongs to is a property of which port it
/// arrived on — information this function does not have and must not
/// invent. A caller multiplexing several 1.0 ports onto one UMP endpoint
/// re-stamps the group at the port boundary, where the answer is known.
///
/// The packet carries no timing: the caller pairs it with an offset.
#[must_use]
pub fn from_midi1_bytes(bytes: &[u8]) -> Option<Ump> {
    let message = Midi1Parser::message(MidiGroup::FIRST, bytes)?;
    Some(Ump::from(message))
}

/// Up to three MIDI 1.0 wire messages, each `([status, d1, d2], length)`, from
/// [`to_midi1_messages`]. Allocation-free.
#[derive(Clone, Copy, Debug, Default)]
pub struct Midi1Messages {
    messages: [([u8; 3], u8); 3],
    len: u8,
    next: u8,
}

impl Midi1Messages {
    fn push(&mut self, bytes: [u8; 3], len: u8) {
        self.messages[usize::from(self.len)] = (bytes, len);
        self.len += 1;
    }
}

impl Iterator for Midi1Messages {
    type Item = ([u8; 3], u8);

    fn next(&mut self) -> Option<Self::Item> {
        if self.next < self.len {
            self.next += 1;
            Some(self.messages[usize::from(self.next - 1)])
        } else {
            None
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = usize::from(self.len - self.next);
        (left, Some(left))
    }
}

impl ExactSizeIterator for Midi1Messages {}

/// Emits the MIDI 1.0 wire form of `ump` (1-3 bytes), if the message
/// has a 1.0 representation. Returns `None` for MIDI 2.0-only messages
/// (per-note controllers, RPN/NRPN, utility, SysEx) and non-channel-voice
/// UMP types that don't correspond to a 1.0 status byte.
///
/// One message only. A MIDI 2.0 Program Change that carries a bank is one
/// UMP but three MIDI 1.0 messages (CC 0, CC 32, Program Change); this
/// returns just the Program Change. Use [`to_midi1_messages`]
/// to keep the bank.
#[must_use]
pub fn to_midi1_bytes(ump: &Ump) -> Option<([u8; 3], u8)> {
    let type_nibble = ((ump.0[0] >> 28) & 0x0F) as u8;
    match type_nibble {
        0x2 => cv1_to_midi1_bytes(ump),
        0x4 => cv2_to_midi1_bytes(ump),
        0x1 => system_to_midi1_bytes(ump),
        _ => None,
    }
}

/// The MIDI 1.0 wire messages this packet translates to: none when it has no
/// 1.0 form, one for almost everything, and three for a Program Change with
/// a valid bank (CC 0 = bank MSB, CC 32 = bank LSB, then the Program Change,
/// as M2-104 Appendix D translates it).
#[must_use]
pub fn to_midi1_messages(ump: &Ump) -> Midi1Messages {
    let mut out = Midi1Messages::default();
    // Only System (1), MIDI 1.0 (2) and MIDI 2.0 (4) channel voice packets
    // have a MIDI 1.0 form; the others never reach the `midi2` decoder.
    if !matches!(ump.0[0] >> 28, 0x1 | 0x2 | 0x4) {
        return out;
    }
    if let Some((channel, bank, program)) = program_change_bank(ump) {
        out.push([0xB0 | channel, 0, ((bank >> 7) & 0x7F) as u8], 3);
        out.push([0xB0 | channel, 32, (bank & 0x7F) as u8], 3);
        out.push([0xC0 | channel, program, 0], 2);
    } else if let Some((bytes, len)) = to_midi1_bytes(ump) {
        out.push(bytes, len);
    }
    out
}

/// `(channel, 14-bit bank, program)` of a MIDI 2.0 Program Change whose bank
/// is valid.
fn program_change_bank(ump: &Ump) -> Option<(u8, u16, u8)> {
    use midi2::UmpMessage;
    use midi2::channel_voice2::ChannelVoice2;
    let Ok(UmpMessage::ChannelVoice2(ChannelVoice2::ProgramChange(m))) =
        UmpMessage::try_from(ump.words())
    else {
        return None;
    };
    let bank = crate::ump::program_change_bank_of(ump.words())?;
    Some((u8::from(m.channel()), bank, u8::from(m.program())))
}

fn cv1_to_midi1_bytes(ump: &Ump) -> Option<([u8; 3], u8)> {
    let opcode = ((ump.0[0] >> 20) & 0x0F) as u8;
    let channel = ((ump.0[0] >> 16) & 0x0F) as u8;
    let d1 = ((ump.0[0] >> 8) & 0x7F) as u8;
    let d2 = (ump.0[0] & 0x7F) as u8;
    let status = (opcode << 4) | channel;
    let len = match opcode {
        0x8 | 0x9 | 0xA | 0xB | 0xE => 3,
        0xC | 0xD => 2,
        _ => return None,
    };
    Some(([status, d1, d2], len))
}

fn cv2_to_midi1_bytes(ump: &Ump) -> Option<([u8; 3], u8)> {
    use super::scaling::{midi2_cc_to_midi1, midi2_pitch_bend_to_midi1, midi2_velocity_to_midi1};
    use midi2::UmpMessage;
    use midi2::channel_voice2::ChannelVoice2;
    let msg = UmpMessage::try_from(ump.words()).ok()?;
    let UmpMessage::ChannelVoice2(cv2) = msg else {
        return None;
    };
    let channel = u8::from(cv2.channel());
    let (opcode, d1, d2, len): (u8, u8, u8, u8) = match cv2 {
        ChannelVoice2::NoteOn(m) => (
            0x9,
            u8::from(m.note_number()),
            // M2-104-UM §7.4.2: "When translating a MIDI 2.0 Note On
            // message to the MIDI 1.0 Protocol, if the translated MIDI 1.0
            // value of the Velocity is zero, then the Translator shall
            // replace the zero with a value of 1." Downscale is `>> 9`, so
            // every CV2 velocity in 0x0001..=0x01FF lands on 0 — and a
            // MIDI 1.0 Note On with velocity 0 *is* a Note Off, which would
            // silence the note instead of sounding it.
            midi2_velocity_to_midi1(m.velocity()).max(1),
            3,
        ),
        ChannelVoice2::NoteOff(m) => (
            0x8,
            u8::from(m.note_number()),
            midi2_velocity_to_midi1(m.velocity()),
            3,
        ),
        ChannelVoice2::ControlChange(m) => (
            0xB,
            u8::from(m.control()),
            midi2_cc_to_midi1(m.control_change_data()),
            3,
        ),
        ChannelVoice2::ChannelPitchBend(m) => {
            let bend14 = midi2_pitch_bend_to_midi1(m.pitch_bend_data());
            (0xE, (bend14 & 0x7F) as u8, ((bend14 >> 7) & 0x7F) as u8, 3)
        }
        ChannelVoice2::ProgramChange(m) => (0xC, u8::from(m.program()), 0, 2),
        ChannelVoice2::ChannelPressure(m) => {
            (0xD, midi2_cc_to_midi1(m.channel_pressure_data()), 0, 2)
        }
        ChannelVoice2::KeyPressure(m) => (
            0xA,
            u8::from(m.note_number()),
            midi2_cc_to_midi1(m.key_pressure_data()),
            3,
        ),
        // Per-note pitch bend, per-note controller, RPN/NRPN have no
        // 1.0 single-message equivalent.
        _ => return None,
    };
    let status = (opcode << 4) | (channel & 0x0F);
    Some(([status, d1, d2], len))
}

fn system_to_midi1_bytes(ump: &Ump) -> Option<([u8; 3], u8)> {
    let status = ((ump.0[0] >> 16) & 0xFF) as u8;
    let d1 = ((ump.0[0] >> 8) & 0x7F) as u8;
    let d2 = (ump.0[0] & 0x7F) as u8;
    match status {
        0xF1 => Some(([status, d1, 0], 2)),
        0xF2 => Some(([status, d1, d2], 3)),
        0xF3 => Some(([status, d1, 0], 2)),
        0xF6 | 0xF8 | 0xFA..=0xFC | 0xFE | 0xFF => Some(([status, 0, 0], 1)),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
