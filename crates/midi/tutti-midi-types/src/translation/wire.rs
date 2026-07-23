//! MIDI 1.0 wire format ↔ UMP conversion: parse 1-3 byte MIDI-1 messages into a
//! [`MidiEvent`] and emit the MIDI-1 wire form back out.

use midi2::prelude::*;

use crate::ump::MidiEvent;

/// The bytes were not a parseable MIDI 1.0 channel-voice or system message
/// (malformed, or a SysEx — use [`MidiEvent::sysex7_fragments`] for that).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MidiParseError;

impl core::fmt::Display for MidiParseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("not a parseable MIDI 1.0 message")
    }
}

impl std::error::Error for MidiParseError {}

impl TryFrom<&[u8]> for MidiEvent {
    type Error = MidiParseError;

    /// Parse MIDI 1.0 wire bytes at frame offset 0 — the idiomatic spelling of
    /// [`MidiEvent::from_midi1_bytes`]. Use `from_midi1_bytes` when you need a
    /// non-zero frame offset.
    #[inline]
    fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
        Self::from_midi1_bytes(0, bytes).ok_or(MidiParseError)
    }
}

impl MidiEvent {
    /// Parse raw MIDI 1.0 wire bytes (2-3 byte channel-voice or 1-byte system
    /// real-time message) into a UMP [`MidiEvent`] of type 0x2 (Channel Voice 1)
    /// or 0x1 (System). Returns `None` on malformed input or SysEx (use
    /// [`Self::sysex7_fragments`] for that).
    pub fn from_midi1_bytes(frame_offset: u32, bytes: &[u8]) -> Option<Self> {
        use midly::live::{LiveEvent, SystemRealtime};
        let ev = LiveEvent::parse(bytes).ok()?;
        let out = match ev {
            LiveEvent::Midi { channel, message } => {
                midi1_channel_voice_to_ump(channel.as_int(), message)
            }
            LiveEvent::Realtime(rt) => match rt {
                SystemRealtime::TimingClock => Self::timing_clock(0),
                SystemRealtime::Start => Self::start(0),
                SystemRealtime::Continue => Self::continue_msg(0),
                SystemRealtime::Stop => Self::stop(0),
                SystemRealtime::ActiveSensing => Self::active_sensing(0),
                SystemRealtime::Reset => Self::system_reset(0),
                _ => return None,
            },
            LiveEvent::Common(common) => match common {
                midly::live::SystemCommon::MidiTimeCodeQuarterFrame(kind, val) => {
                    let data = (mtc_qf_nibble(kind) << 4) | val.as_int();
                    Self::mtc_quarter_frame(0, data)
                }
                midly::live::SystemCommon::SongPosition(pos) => {
                    Self::song_position(0, pos.as_int())
                }
                midly::live::SystemCommon::SongSelect(song) => Self::song_select(0, song.as_int()),
                midly::live::SystemCommon::TuneRequest => Self::tune_request(0),
                midly::live::SystemCommon::SysEx(_)
                | midly::live::SystemCommon::Undefined(_, _) => {
                    return None;
                }
            },
        };
        Some(out.with_frame_offset(frame_offset))
    }
}

/// Map a midly `MtcQuarterFrameMessage` to its 0-7 piece code (top nibble
/// of the MTC quarter-frame data byte).
fn mtc_qf_nibble(kind: midly::live::MtcQuarterFrameMessage) -> u8 {
    use midly::live::MtcQuarterFrameMessage::*;
    match kind {
        FramesLow => 0,
        FramesHigh => 1,
        SecondsLow => 2,
        SecondsHigh => 3,
        MinutesLow => 4,
        MinutesHigh => 5,
        HoursLow => 6,
        HoursHigh => 7,
    }
}

/// Build a UMP type 0x2 (MIDI 1.0 Channel Voice) event from a midly
/// `MidiMessage`. Keeps the 7-bit data values (no upconversion to MIDI 2.0).
fn midi1_channel_voice_to_ump(channel: u8, msg: midly::MidiMessage) -> MidiEvent {
    use midly::MidiMessage::*;
    let (opcode, d1, d2) = match msg {
        NoteOff { key, vel } => (0x8u32, key.as_int(), vel.as_int()),
        NoteOn { key, vel } => (0x9u32, key.as_int(), vel.as_int()),
        Aftertouch { key, vel } => (0xAu32, key.as_int(), vel.as_int()),
        Controller { controller, value } => (0xBu32, controller.as_int(), value.as_int()),
        ProgramChange { program } => (0xCu32, program.as_int(), 0),
        ChannelAftertouch { vel } => (0xDu32, vel.as_int(), 0),
        PitchBend { bend } => {
            let bend14 = (bend.as_int() as i32 + 8192).clamp(0, 16383) as u32;
            let lsb = (bend14 & 0x7F) as u8;
            let msb = ((bend14 >> 7) & 0x7F) as u8;
            (0xEu32, lsb, msb)
        }
    };
    let w0 = (0x2u32 << 28)
        | (opcode << 20)
        | (((channel & 0x0F) as u32) << 16)
        | (((d1 & 0x7F) as u32) << 8)
        | ((d2 & 0x7F) as u32);
    MidiEvent::from_ump(0, &[w0])
}

impl MidiEvent {
    /// Emit the MIDI 1.0 wire form of this event (1-3 bytes), if the message
    /// has a 1.0 representation. Returns `None` for MIDI 2.0-only messages
    /// (per-note controllers, RPN/NRPN, utility, SysEx) and non-channel-voice
    /// UMP types that don't correspond to a 1.0 status byte.
    pub fn to_midi1_bytes(&self) -> Option<([u8; 3], u8)> {
        let type_nibble = ((self.data[0] >> 28) & 0x0F) as u8;
        match type_nibble {
            0x2 => self.cv1_to_midi1_bytes(),
            0x4 => self.cv2_to_midi1_bytes(),
            0x1 => self.system_to_midi1_bytes(),
            _ => None,
        }
    }

    fn cv1_to_midi1_bytes(&self) -> Option<([u8; 3], u8)> {
        let opcode = ((self.data[0] >> 20) & 0x0F) as u8;
        let channel = ((self.data[0] >> 16) & 0x0F) as u8;
        let d1 = ((self.data[0] >> 8) & 0x7F) as u8;
        let d2 = (self.data[0] & 0x7F) as u8;
        let status = (opcode << 4) | channel;
        let len = match opcode {
            0x8 | 0x9 | 0xA | 0xB | 0xE => 3,
            0xC | 0xD => 2,
            _ => return None,
        };
        Some(([status, d1, d2], len))
    }

    fn cv2_to_midi1_bytes(&self) -> Option<([u8; 3], u8)> {
        use super::scaling::{
            midi2_cc_to_midi1, midi2_pitch_bend_to_midi1, midi2_velocity_to_midi1,
        };
        use midi2::channel_voice2::ChannelVoice2;
        use midi2::UmpMessage;
        let msg = UmpMessage::try_from(self.data_words()).ok()?;
        let UmpMessage::ChannelVoice2(cv2) = msg else {
            return None;
        };
        let channel = u8::from(cv2.channel());
        let (opcode, d1, d2, len): (u8, u8, u8, u8) = match cv2 {
            ChannelVoice2::NoteOn(m) => (
                0x9,
                u8::from(m.note_number()),
                midi2_velocity_to_midi1(m.velocity()),
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

    fn system_to_midi1_bytes(&self) -> Option<([u8; 3], u8)> {
        let status = ((self.data[0] >> 16) & 0xFF) as u8;
        let d1 = ((self.data[0] >> 8) & 0x7F) as u8;
        let d2 = (self.data[0] & 0x7F) as u8;
        match status {
            0xF1 => Some(([status, d1, 0], 2)),
            0xF2 => Some(([status, d1, d2], 3)),
            0xF3 => Some(([status, d1, 0], 2)),
            0xF6 | 0xF8 | 0xFA..=0xFC | 0xFE | 0xFF => Some(([status, 0, 0], 1)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_from_bytes_parses_and_errors() {
        // Note-on wire bytes → a valid MidiEvent at frame 0.
        let ev = MidiEvent::try_from(&[0x93u8, 60, 100][..]).expect("valid note-on");
        assert_eq!(
            ev,
            MidiEvent::from_midi1_bytes(0, &[0x93, 60, 100]).unwrap()
        );
        assert_eq!(ev.frame_offset, 0);
        // Garbage → the typed error.
        assert_eq!(MidiEvent::try_from(&[0x00u8][..]), Err(MidiParseError));
    }
}
