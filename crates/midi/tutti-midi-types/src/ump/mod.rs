//! UMP-based [`MidiEvent`] with sample-accurate timing.
//!
//! A [`MidiEvent`] is a 20-byte packed struct — a 32-bit frame offset plus
//! four UMP words — carrying any MIDI message type. Construction goes through
//! [`midi2`] (spec-compliant encoding).
//!
//! **To decode, prefer [`MidiEvent::message`](crate::MidiMessage) → a
//! [`MidiMessage`](crate::MidiMessage)** — the app-facing "just tell me what it
//! is" view with `.note()` / `.channel()` / `.velocity()` accessors, already
//! normalized so a MIDI 1.0 event arrives as its MIDI 2.0 form. Only reach for
//! the raw `midi2` layer below when you need a message family `MidiMessage`
//! doesn't model:
//!
//! ```ignore
//! use midi2::UmpMessage;
//! if let Ok(msg) = UmpMessage::try_from(ev.data_words()) {
//!     // pattern match on the raw midi2 message
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

    /// The UMP Message Type — the top nibble of word 0 (MIDI 2.0 spec §2.1.4).
    /// A cheap classifier for routing a raw event without a full `midi2` decode
    /// (e.g. "is this a SysEx7 packet I should reassemble?").
    #[inline]
    pub fn message_type(&self) -> UmpMessageType {
        UmpMessageType::from_nibble((self.data[0] >> 28) as u8)
    }

    /// The UMP group (0–15) — bits 24–27 of word 0. Meaningful for the
    /// group-scoped message types (channel voice, SysEx, Flex Data); the
    /// group-less types (Utility, UMP Stream) ignore it.
    #[inline]
    pub fn group(&self) -> u8 {
        ((self.data[0] >> 24) & 0x0F) as u8
    }
}

/// A UMP Message Type (the top nibble of word 0), as a named classifier
/// (MIDI 2.0 spec §2.1.4). Only the types tutti routes on are named; the rest
/// fold into [`Other`](UmpMessageType::Other) carrying the raw nibble.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UmpMessageType {
    /// 0x1 — System Real Time / System Common.
    System,
    /// 0x2 — MIDI 1.0 Channel Voice.
    ChannelVoice1,
    /// 0x3 — Data (64-bit): SysEx7.
    Sysex7,
    /// 0x4 — MIDI 2.0 Channel Voice.
    ChannelVoice2,
    /// 0x5 — Data (128-bit): SysEx8 / Mixed Data.
    Sysex8,
    /// 0xD — Flex Data.
    FlexData,
    /// 0xF — UMP Stream.
    UmpStream,
    /// Any other message type, carrying its raw nibble.
    Other(u8),
}

impl UmpMessageType {
    #[inline]
    fn from_nibble(nibble: u8) -> Self {
        match nibble & 0x0F {
            0x1 => Self::System,
            0x2 => Self::ChannelVoice1,
            0x3 => Self::Sysex7,
            0x4 => Self::ChannelVoice2,
            0x5 => Self::Sysex8,
            0xD => Self::FlexData,
            0xF => Self::UmpStream,
            other => Self::Other(other),
        }
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

/// Word count per UMP message type nibble (M2-104-UM §2.1.4, Table 4).
///
/// The reserved types carry sizes too — Table 4 fixes them precisely so a
/// parser can skip a message type it does not understand without losing the
/// rest of the stream. 0xB and 0xC are **96 bits** (3 words), not 128; getting
/// this wrong desynchronizes every following message in the buffer.
#[inline]
pub(crate) const fn ump_word_count(type_nibble: u8) -> usize {
    match type_nibble & 0x0F {
        0x0 | 0x1 | 0x2 | 0x6 | 0x7 => 1,
        0x3 | 0x4 | 0x8 | 0x9 | 0xA => 2,
        0xB | 0xC => 3,
        0x5 | 0xD | 0xE | 0xF => 4,
        _ => 1,
    }
}

/// Split a packed UMP word stream into its individual messages.
///
/// A native-UMP transport (CoreMIDI's `MIDIEventPacket`, a MIDI-2.0 USB packet)
/// delivers *several* concatenated messages in one buffer, with no separators —
/// each message's length is implied by its type nibble (§2.1.3). This walks the
/// stream by that length and yields one [`MidiEvent`] per message.
///
/// A trailing run too short for its declared type is dropped: it is a truncated
/// message, not a decodable one. Every yielded event carries `frame_offset` 0;
/// the caller stamps timing from its own transport if it needs to.
pub fn split_ump_stream(words: &[u32]) -> impl Iterator<Item = MidiEvent> + '_ {
    let mut idx = 0usize;
    core::iter::from_fn(move || {
        if idx >= words.len() {
            return None;
        }
        let n = ump_word_count((words[idx] >> 28) as u8);
        if idx + n > words.len() {
            // Truncated tail — nothing decodable remains.
            idx = words.len();
            return None;
        }
        let event = MidiEvent::from_ump(0, &words[idx..idx + n]);
        idx += n;
        Some(event)
    })
}

// UMP message families — each extends `MidiEvent` in its own module. The MIDI
// 1.0 wire codec (`from/to_midi1_bytes`) is a `MidiEvent` family too, but it
// lives in `crate::midi1` alongside the rest of the MIDI-1 boundary.
mod channel_voice;
mod controllers;
mod flex_data;
mod stream;
mod sysex;
mod sysex8;
mod system;
mod utility;

pub use controllers::{
    RPN_BANK_MPE, RPN_INDEX_CHANNEL_PITCH_BEND_SENSITIVITY, RPN_INDEX_MCM,
    RPN_INDEX_PER_NOTE_PITCH_BEND_SENSITIVITY,
};
pub use flex_data::{
    bpm_to_ten_ns_per_quarter, flex_chord_name, flex_key_signature, flex_tempo_bpm,
    flex_time_signature, flex_text,
    push_flex_text, ten_ns_per_quarter_to_bpm, Alteration, BarAccents, ChordBass, ChordName,
    ChordSharpsFlats, ChordType, FlexTextKind, KeySharpsFlats, Tonic,
};
pub use stream::{
    endpoint_name, product_instance_id, EndpointCapabilities, EndpointDiscoveryRequest,
    FunctionBlockDirection, FunctionBlocks, JrTimestamps, Protocol, UmpVersion,
};
pub use sysex::{
    SYSEX7_STATUS_CONTINUE, SYSEX7_STATUS_END, SYSEX7_STATUS_SINGLE, SYSEX7_STATUS_START,
};
pub use sysex8::{
    sysex8_message, SYSEX8_STATUS_CONTINUE, SYSEX8_STATUS_END, SYSEX8_STATUS_SINGLE,
    SYSEX8_STATUS_START,
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

    #[test]
    fn split_ump_stream_walks_mixed_word_lengths() {
        // A native-UMP packet concatenates messages of different lengths with no
        // separators: 1-word JR Timestamp, 2-word CV2 note-on, 1-word clock.
        let jr = MidiEvent::jr_timestamp(0x1234);
        let note = MidiEvent::note_on(0, 3, 60, 0x8000);
        let clock = MidiEvent::timing_clock(0);

        let mut words = Vec::new();
        words.extend_from_slice(jr.data_words());
        words.extend_from_slice(note.data_words());
        words.extend_from_slice(clock.data_words());
        assert_eq!(words.len(), 4, "1 + 2 + 1 words");

        let split: Vec<_> = split_ump_stream(&words).collect();
        assert_eq!(split, vec![jr, note, clock]);
    }

    #[test]
    fn split_ump_stream_drops_a_truncated_tail() {
        // A 2-word CV2 note-on with only its first word present is not decodable.
        let note = MidiEvent::note_on(0, 0, 60, 0x8000);
        let words = [note.data_words()[0]];
        assert_eq!(split_ump_stream(&words).count(), 0);

        // …but a complete message before the truncated tail still comes out.
        let clock = MidiEvent::timing_clock(0);
        let words = [clock.data_words()[0], note.data_words()[0]];
        assert_eq!(split_ump_stream(&words).collect::<Vec<_>>(), vec![clock]);
    }

    #[test]
    fn split_ump_stream_is_empty_for_no_words() {
        assert_eq!(split_ump_stream(&[]).count(), 0);
    }

    #[test]
    fn reserved_message_types_have_their_table_4_sizes() {
        // M2-104-UM §2.1.4 Table 4. The reserved types carry fixed sizes
        // precisely so a parser can skip them without losing the stream, so
        // these are as load-bearing as the defined ones. 0xB/0xC are 96 bits.
        for (mt, words) in [
            (0x0, 1),
            (0x1, 1),
            (0x2, 1),
            (0x3, 2),
            (0x4, 2),
            (0x5, 4),
            (0x6, 1),
            (0x7, 1),
            (0x8, 2),
            (0x9, 2),
            (0xA, 2),
            (0xB, 3),
            (0xC, 3),
            (0xD, 4),
            (0xE, 4),
            (0xF, 4),
        ] {
            assert_eq!(ump_word_count(mt), words, "MT {mt:#x}");
        }
    }

    #[test]
    fn a_reserved_type_packet_does_not_desync_the_stream() {
        // An MT-0xB packet (3 words) followed by a real message: if the walker
        // takes 4 words for the 0xB, it eats the note-on's first word and every
        // message after it decodes as garbage.
        let note = MidiEvent::note_on(0, 0, 60, 0x8000);
        let mut words = vec![0xB000_0000u32, 0, 0];
        words.extend_from_slice(note.data_words());

        let split: Vec<_> = split_ump_stream(&words).collect();
        assert_eq!(split.len(), 2, "reserved packet, then the note-on");
        assert_eq!(split[1], note, "the note-on survives intact");
    }
}
