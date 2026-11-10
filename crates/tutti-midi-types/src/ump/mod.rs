//! The UMP surface: multi-packet builders and readers over [`Ump`], and the
//! classifiers a router needs.
//!
//! There is no event type here: an event in a block is `(Offset, Ump)` (the
//! caller pairs the offset), and a single message is built with
//! `tutti_ump::Message` and `From<Message> for Ump`. What stays in this module
//! is what a `Message` does not carry: SysEx7/SysEx8 fragmentation, Flex Data,
//! UMP Stream, and the readers that go with them.
//!
//! **To decode, prefer [`MidiMessage::from_ump`](crate::MidiMessage::from_ump)**,
//! the app-facing "just tell me what it is" view with `.note()` /
//! `.channel()` / `.velocity()` accessors, already normalized so a MIDI 1.0
//! event arrives as its MIDI 2.0 form. Only reach for `tutti_ump::decode` or
//! the raw `midi2` layer when you need a message family `MidiMessage` doesn't
//! model:
//!
//! ```
//! use tutti_midi_types::midi2::{channel_voice2::ChannelVoice2, UmpMessage};
//! use tutti_ump::{Address, Attribute, Message, MidiChannel, MidiGroup, NoteNumber, Ump, Voice2};
//!
//! let ump = Ump::from(Message::Midi2(
//!     Address { group: MidiGroup::FIRST, channel: MidiChannel::FIRST },
//!     Voice2::NoteOn { note: NoteNumber::new(60).unwrap(), velocity: 0x8000, attribute: Attribute::None },
//! ));
//! match UmpMessage::try_from(ump.words()) {
//!     Ok(UmpMessage::ChannelVoice2(ChannelVoice2::NoteOn(m))) => {
//!         assert_eq!(m.velocity(), 0x8000);
//!     }
//!     _ => panic!("a note-on built here decodes as one"),
//! }
//! ```

use tutti_ump::Ump;

/// The UMP Message Type — the top nibble of word 0 (MIDI 2.0 spec §2.1.4). A
/// cheap classifier for routing a raw packet without a full decode (e.g. "is
/// this a SysEx7 packet I should reassemble?").
#[inline]
#[must_use]
pub fn message_type(ump: &Ump) -> UmpMessageType {
    UmpMessageType::from_nibble((ump.0[0] >> 28) as u8)
}

/// A packet from the meaningful words of one message, zero-padded to four.
pub(crate) fn packet_from(words: &[u32]) -> Ump {
    let mut packet = [0u32; 4];
    let n = words.len().min(4);
    packet[..n].copy_from_slice(&words[..n]);
    Ump(packet)
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

/// Splits a packed UMP word stream into its individual messages.
///
/// A native-UMP transport (CoreMIDI's `MIDIEventPacket`, a MIDI-2.0 USB packet)
/// delivers *several* concatenated messages in one buffer, with no separators —
/// each message's length is implied by its type nibble (§2.1.3). This walks the
/// stream by that length and yields one [`Ump`] per message.
///
/// A trailing run too short for its declared type is dropped: it is a truncated
/// message, not a decodable one. The packets carry no timing; the caller pairs
/// each with an offset from its own transport if it needs one.
pub fn split_ump_stream(words: &[u32]) -> impl Iterator<Item = Ump> + '_ {
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
        let mut packet = [0u32; 4];
        packet[..n].copy_from_slice(&words[idx..idx + n]);
        idx += n;
        Some(Ump(packet))
    })
}

// UMP message families, each a module of free functions over `Ump`. The MIDI
// 1.0 wire codec (`from/to_midi1_bytes`) lives in `crate::translation::wire`
// alongside the rest of the MIDI-1 boundary.
mod channel_voice;
mod controllers;
mod flex_data;
mod stream;
mod sysex;
mod sysex8;
mod system;
mod utility;

pub(crate) use channel_voice::program_change_bank_of;
pub use controllers::{
    RPN_BANK_MPE, RPN_INDEX_CHANNEL_PITCH_BEND_SENSITIVITY, RPN_INDEX_MCM,
    RPN_INDEX_PER_NOTE_PITCH_BEND_SENSITIVITY,
};
pub use flex_data::{
    Alteration, BarAccents, ChordBass, ChordName, ChordSharpsFlats, ChordType, FlexTextKind,
    FlexTextReassembler, KeySharpsFlats, Tonic, bpm_to_ten_ns_per_quarter, flex_chord_name,
    flex_key_signature, flex_set_chord_name, flex_set_key_signature, flex_set_metronome,
    flex_set_tempo, flex_set_time_signature, flex_tempo_bpm, flex_text, flex_time_signature,
    push_flex_text, ten_ns_per_quarter_to_bpm,
};
pub use stream::{
    ALL_FUNCTION_BLOCKS, EndpointCapabilities, EndpointDiscoveryRequest, FunctionBlockDirection,
    FunctionBlockDiscoveryRequest, FunctionBlocks, JrTimestamps, Protocol, UmpVersion,
    device_identity, endpoint_discovery, endpoint_info, endpoint_name, function_block_discovery,
    function_block_info, function_block_name, product_instance_id,
    stream_configuration_notification, stream_configuration_request,
};
pub use sysex::{
    SYSEX7_MAX_PAYLOAD, SYSEX7_STATUS_CONTINUE, SYSEX7_STATUS_END, SYSEX7_STATUS_SINGLE,
    SYSEX7_STATUS_START, sysex7_fragments, sysex7_packet_count, sysex7_packets, sysex7_payload,
    sysex7_single,
};
pub use sysex8::{
    SYSEX8_BYTES_ABORT, SYSEX8_STATUS_CONTINUE, SYSEX8_STATUS_END, SYSEX8_STATUS_SINGLE,
    SYSEX8_STATUS_START, is_sysex8_abort, sysex8_fragments, sysex8_header, sysex8_message,
    sysex8_status,
};
pub use utility::{jr_clock_value, jr_timestamp_value};

#[cfg(test)]
mod tests;
