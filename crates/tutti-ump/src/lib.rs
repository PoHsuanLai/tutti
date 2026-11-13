#![doc = include_str!("../README.md")]
//! The Universal MIDI Packet (M2-104): its storage, its addresses and its
//! messages.
//!
//! [`Ump`] is one packet as four `u32` words. [`MidiGroup`], [`MidiChannel`]
//! and [`CCNumber`] are the addresses a packet carries, as newtypes so that a
//! transposed `(group, channel)` pair does not compile. [`Message`] is one
//! defined message as a `Copy` value with no lifetime. [`Ump::decode`]
//! (and `TryFrom<Ump> for Message`) and `From<Message> for Ump` hold the bit positions of every message; the one
//! other reader is `Ump::is_note_off` (and `Ump::words`, which reads the type
//! nibble), which a test ties to `Ump::decode`. MIDI 1.0 on the wire is the other
//! place that knows bit positions: [`Voice1::bytes`] and [`System::bytes`] write
//! a message's bytes, [`frame`] adds a SysEx's delimiters, and [`Midi1Parser`]
//! reads a byte stream into [`Message`]s. [`NoteId`] is a note's identity
//! (`channel * 128 + note`, the plugin formats' wire value), and [`SysexMessage`]
//! and [`SysexAssembler`] are a checked complete SysEx and the one SysEx7
//! reassembler (a stream per Group), over a buffer the caller owns; [`SysexMessage::packets`] is its inverse,
//! the one writer of a payload as SysEx7 packets ([`Sysex7Packets::from_payload`] for plain bytes), as [`flex_text_packets`] is of a text as Flex
//! Data packets. [`split_stream`] cuts a buffer
//! of concatenated packets (a native-UMP transport's) into [`Ump`]s.
//!
//! ```
//! use tutti_ump::{Message, Ump, Voice2};
//!
//! // MIDI 2.0 Note On: group 0, channel 0, note 60, 16-bit velocity 0x8000.
//! let packet = Ump([0x4090_3C00, 0x8000_0000, 0, 0]);
//! let Ok(Message::Midi2(address, Voice2::NoteOn { note, velocity, .. })) = packet.decode() else {
//!     unreachable!()
//! };
//! assert_eq!((address.channel.get(), note.get(), velocity.get()), (0, 60, 0x8000));
//! ```
//!
//! Decoding reads no reserved bit. A reserved value in an enumerated field is
//! kept raw, so a later revision's message stays readable and re-encodes
//! exactly; a packet that cannot be framed comes back as [`Unknown`] with its
//! words, and a consumer forwards it unchanged, never drops it. A packet
//! re-encodes to itself when it is canonical: its reserved bits are zero, and a
//! field another field makes meaningless has its canonical value (attribute
//! data under type 0, bank bytes under a clear Bank Valid bit, and a chord bass
//! accidental under bass note 0, which is -8). A MIDI 1.0 Note On at velocity 0 is a Note On here;
//! reading it as a Note Off is translation's, and the same for scaling between
//! the two protocols.
//!
//! # Messages are values
//!
//! A [`Message`] has no lifetime: it can be matched, stored or queued on any
//! thread with no allocation. Every message M2-104 defines is one packet of at
//! most 16 bytes, so text and SysEx chunks are small arrays in their variants;
//! a whole string or SysEx message is reassembled above the codec, off the audio
//! thread, by [`FlexTextAssembler`] and [`SysexAssembler`]. The limits on a
//! reassembled name (Endpoint Name, Function Block Name, Product Instance Id)
//! are not the codec's: `tutti-midi-device` checks them where it builds a name.
//!
//! The fields are typed ([`NoteNumber`], [`U4`], [`U7`], [`U14`], [`Pitch`] and
//! the address types). A public `new` refuses an out-of-range raw, `of` builds
//! one from a literal or constant and panics out of range, and `masked` is the
//! one constructor that masks, for bits a decoder has just framed. Only the
//! codec shifts bits: a consumer matches on [`Message`], and the graph's queues
//! call [`Ump::is_note_off`].
//!
//! The crate depends on no other tutti crate and has no allocation, so the
//! audio graph can carry packets without linking a MIDI stack.

#![forbid(unsafe_code)]

mod bend_value;
mod cc_number;
mod control_value;
mod decode;
mod encode;
mod field;
mod flex_text;
mod manufacturer;
mod message;
mod midi1;
mod midi_channel;
mod midi_group;
mod note_id;
mod note_velocity;
mod rpn;
mod sysex;
mod ump;

pub use bend_value::BendValue;
pub use cc_number::CCNumber;
pub use control_value::ControlValue;
pub use field::{
    DeltaTicks, FirstGroup, FunctionBlockId, GroupSpan, NoteNumber, OutOfRange, Pitch, SharpsFlats,
    U4, U7, U14,
};
pub use flex_text::{
    FlexText, FlexTextAssembler, FlexTextPackets, FlexTextStep, flex_text_packets,
};
pub use manufacturer::{ManufacturerId, NotManufacturerId};
pub use message::{
    Accidental, Address, Alteration, AlterationKind, Attribute, Bank, BlockFilter, BlockSelect,
    ChordBass, ChordName, ChordPitch, ChordType, Data128, DeviceIdentity, Direction,
    DiscoveryFilter, EndpointInfo, Flex, FlexAddress, FlexTextKind, Form, FunctionBlockInfo,
    KeySignature, Letter, Message, Metronome, Midi1Port, MixedDataHeader, Protocol,
    RegisteredPerNoteController, Stream, StreamConfig, Sysex7Packet, Sysex8Packet, SysexStatus,
    System, TimeSignature, UiHint, UmpVersion, Unknown, Utility, Voice1, Voice2,
};
pub use midi_channel::MidiChannel;
pub use midi_group::MidiGroup;
pub use midi1::{Framed, Midi1Bytes, Midi1Parser, frame, parse_exact};
pub use note_id::NoteId;
pub use note_velocity::NoteVelocity;
pub use rpn::Rpn;
pub use sysex::{
    SYSEX7_MAX_PAYLOAD, Sysex7Packets, SysexAssembler, SysexError, SysexMessage, SysexRef,
    SysexStep,
};
pub use ump::{Key, KeyEvent, Ump, split_stream};
