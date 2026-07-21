//! UMP-based [`MidiEvent`] with sample-accurate timing.
//!
//! A [`MidiEvent`] is a 20-byte packed struct — a 32-bit frame offset plus
//! four UMP words — carrying any MIDI message type. Construction goes through
//! [`midi2`] (spec-compliant encoding). Decoding is a `TryFrom` into
//! [`midi2::UmpMessage`] via [`MidiEvent::data_words`]:
//!
//! ```ignore
//! use midi2::UmpMessage;
//! if let Ok(msg) = UmpMessage::try_from(ev.data_words()) {
//!     // pattern match on msg
//! }
//! ```

/// Packed UMP event with sample-accurate timing.
///
/// The `data` field is a midi2-compatible `[u32; 4]` buffer — callers with a
/// `midi2::UmpMessage` in hand can simply copy its `.data()` into the first
/// N words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct MidiEvent {
    /// Offset within the current audio buffer, in samples.
    pub frame_offset: u32,
    /// Raw UMP words. [`Self::data_words`] returns the meaningful prefix.
    pub data: [u32; 4],
}

impl MidiEvent {
    /// Construct with pre-built UMP words and a frame offset.
    ///
    /// `words` must be 1, 2, or 4 entries long matching the UMP message type
    /// in the first word's top nibble. Extra slots in `data` are zero-padded.
    #[inline]
    pub fn from_ump(frame_offset: u32, words: &[u32]) -> Self {
        let mut data = [0u32; 4];
        let n = words.len().min(4);
        data[..n].copy_from_slice(&words[..n]);
        Self { frame_offset, data }
    }

    /// Builder-style setter used in chain form, e.g.
    /// `MidiEvent::note_on(...).with_frame_offset(128)`.
    #[inline]
    #[must_use]
    pub fn with_frame_offset(mut self, frame_offset: u32) -> Self {
        self.frame_offset = frame_offset;
        self
    }

    /// Return the meaningful prefix of [`Self::data`] (1, 2, or 4 words)
    /// per the UMP spec. Hand directly to `midi2::UmpMessage::try_from`.
    #[inline]
    pub fn data_words(&self) -> &[u32] {
        let n = ump_word_count((self.data[0] >> 28) as u8);
        &self.data[..n]
    }
}

// -----------------------------------------------------------------------------
// Quick accessors — thin passthroughs over midi2 for fast note/CC lookup
// -----------------------------------------------------------------------------
//
// Callers that need more than these (per-note pitch bend, program change bank,
// sysex, etc.) match `UmpMessage::try_from(ev.data_words())` directly. These
// cover the note-on/off + velocity lookup that MPE, voice allocators, and
// tests repeatedly need.

impl MidiEvent {
    /// `true` if this is a Channel Voice 1 or 2 Note On with non-zero velocity.
    pub fn is_note_on(&self) -> bool {
        use midi2::channel_voice1::ChannelVoice1;
        use midi2::channel_voice2::ChannelVoice2;
        use midi2::UmpMessage;
        match UmpMessage::try_from(self.data_words()) {
            Ok(UmpMessage::ChannelVoice2(ChannelVoice2::NoteOn(m))) => m.velocity() > 0,
            Ok(UmpMessage::ChannelVoice1(ChannelVoice1::NoteOn(m))) => u8::from(m.velocity()) > 0,
            _ => false,
        }
    }

    /// `true` if this is a Note Off (or MIDI 1.0 velocity-0 NoteOn).
    pub fn is_note_off(&self) -> bool {
        use midi2::channel_voice1::ChannelVoice1;
        use midi2::channel_voice2::ChannelVoice2;
        use midi2::UmpMessage;
        match UmpMessage::try_from(self.data_words()) {
            Ok(UmpMessage::ChannelVoice2(ChannelVoice2::NoteOff(_))) => true,
            Ok(UmpMessage::ChannelVoice1(ChannelVoice1::NoteOff(_))) => true,
            Ok(UmpMessage::ChannelVoice1(ChannelVoice1::NoteOn(m))) => u8::from(m.velocity()) == 0,
            _ => false,
        }
    }

    /// Note number for note-on/off/poly-pressure/per-note events.
    pub fn note(&self) -> Option<u8> {
        use midi2::channel_voice1::ChannelVoice1;
        use midi2::channel_voice2::ChannelVoice2;
        use midi2::UmpMessage;
        match UmpMessage::try_from(self.data_words()).ok()? {
            UmpMessage::ChannelVoice2(ChannelVoice2::NoteOn(m)) => Some(u8::from(m.note_number())),
            UmpMessage::ChannelVoice2(ChannelVoice2::NoteOff(m)) => Some(u8::from(m.note_number())),
            UmpMessage::ChannelVoice2(ChannelVoice2::KeyPressure(m)) => {
                Some(u8::from(m.note_number()))
            }
            UmpMessage::ChannelVoice2(ChannelVoice2::PerNotePitchBend(m)) => {
                Some(u8::from(m.note_number()))
            }
            UmpMessage::ChannelVoice1(ChannelVoice1::NoteOn(m)) => Some(u8::from(m.note_number())),
            UmpMessage::ChannelVoice1(ChannelVoice1::NoteOff(m)) => Some(u8::from(m.note_number())),
            UmpMessage::ChannelVoice1(ChannelVoice1::KeyPressure(m)) => {
                Some(u8::from(m.note_number()))
            }
            _ => None,
        }
    }

    /// Velocity as a 7-bit value (downconverted from MIDI 2.0's 16-bit form
    /// via spec Min-Center-Max).
    pub fn velocity_u7(&self) -> Option<u8> {
        use crate::convert::midi2_velocity_to_midi1;
        use midi2::channel_voice1::ChannelVoice1;
        use midi2::channel_voice2::ChannelVoice2;
        use midi2::UmpMessage;
        match UmpMessage::try_from(self.data_words()).ok()? {
            UmpMessage::ChannelVoice2(ChannelVoice2::NoteOn(m)) => {
                Some(midi2_velocity_to_midi1(m.velocity()))
            }
            UmpMessage::ChannelVoice2(ChannelVoice2::NoteOff(m)) => {
                Some(midi2_velocity_to_midi1(m.velocity()))
            }
            UmpMessage::ChannelVoice1(ChannelVoice1::NoteOn(m)) => Some(u8::from(m.velocity())),
            UmpMessage::ChannelVoice1(ChannelVoice1::NoteOff(m)) => Some(u8::from(m.velocity())),
            _ => None,
        }
    }

    /// Channel nibble (0-15) for a channel-voice event, read directly from the
    /// UMP word without a full decode. `None` for system, SysEx, and utility
    /// messages, which carry no channel. Covers both MIDI 1.0 (UMP type 0x2)
    /// and MIDI 2.0 (type 0x4) channel voice — the channel sits in the same
    /// bit position in both, so the hot path (e.g. MIDI routing by channel)
    /// avoids paying for a `midi2::UmpMessage::try_from` dispatch.
    #[inline]
    pub fn channel(&self) -> Option<u8> {
        let type_nibble = (self.data[0] >> 28) & 0x0F;
        // UMP type 0x2 = MIDI 1.0 channel voice, 0x4 = MIDI 2.0 channel voice.
        if type_nibble == 0x2 || type_nibble == 0x4 {
            Some(((self.data[0] >> 16) & 0x0F) as u8)
        } else {
            None
        }
    }
}

// -----------------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------------

/// Word count per UMP message type nibble (MIDI 2.0 spec §2.1.3).
#[inline]
pub(crate) const fn ump_word_count(type_nibble: u8) -> usize {
    match type_nibble & 0x0F {
        0x0 | 0x1 | 0x2 | 0x6 | 0x7 => 1,
        0x3 | 0x4 | 0x8 | 0x9 | 0xA => 2,
        0x5 | 0xB | 0xC | 0xD | 0xE | 0xF => 4,
        _ => 1,
    }
}

// UMP message families — each extends `MidiEvent` in its own module.
mod channel_voice;
mod controllers;
mod flex_data;
mod midi1;
mod stream;
mod sysex;
mod system;
mod utility;

pub use controllers::{RPN_BANK_MPE, RPN_INDEX_MCM, RPN_INDEX_PITCH_BEND_SENSITIVITY};
pub use flex_data::{
    bpm_to_ten_ns_per_quarter, flex_tempo_bpm, ten_ns_per_quarter_to_bpm, BarAccents,
};
pub use stream::{
    EndpointCapabilities, EndpointDiscoveryRequest, FunctionBlockDirection, FunctionBlocks,
    JrTimestamps, Protocol, UmpVersion,
};
pub use sysex::{
    SYSEX7_STATUS_CONTINUE, SYSEX7_STATUS_END, SYSEX7_STATUS_SINGLE, SYSEX7_STATUS_START,
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Layout contract: `#[repr(C)] { u32, [u32;4] }` is exactly 20 bytes.
    #[test]
    fn layout_is_20_bytes() {
        assert_eq!(core::mem::size_of::<MidiEvent>(), 20);
    }

    #[test]
    fn with_frame_offset_preserves_payload() {
        let ev = MidiEvent::note_on(0, 0, 60, 0x8000);
        let shifted = ev.with_frame_offset(128);
        assert_eq!(shifted.frame_offset, 128);
        assert_eq!(shifted.data, ev.data);
    }

    #[test]
    fn channel_reads_both_voice_versions() {
        // MIDI 2.0 channel voice (UMP type 0x4).
        assert_eq!(MidiEvent::note_on(0, 5, 60, 0x8000).channel(), Some(5));
        // MIDI 1.0 channel voice (UMP type 0x2), built via the wire bridge.
        let cv1 = MidiEvent::from_midi1_bytes(0, &[0x93, 0x3C, 0x64]).unwrap();
        assert_eq!(cv1.channel(), Some(3));
        // System messages carry no channel.
        assert_eq!(MidiEvent::timing_clock(0).channel(), None);
        assert_eq!(MidiEvent::noop().channel(), None);
    }
}
