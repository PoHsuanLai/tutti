//! The Universal MIDI Packet (M2-104): its storage, its addresses and its
//! messages.
//!
//! [`Ump`] is one packet as four `u32` words. [`MidiGroup`], [`MidiChannel`]
//! and [`CCNumber`] are the addresses a packet carries, as newtypes so that a
//! transposed `(group, channel)` pair does not compile. [`Message`] is one
//! defined message as a `Copy` value with no lifetime. [`decode()`](fn@decode)
//! and `From<Message> for Ump` hold the bit positions of every message; the one
//! other reader is `Ump::is_note_off` (and `Ump::words`, which reads the type
//! nibble), which a test ties to `decode`.
//!
//! ```
//! use tutti_ump::{Message, Ump, Voice2, decode};
//!
//! // MIDI 2.0 Note On: group 0, channel 0, note 60, 16-bit velocity 0x8000.
//! let packet = Ump([0x4090_3C00, 0x8000_0000, 0, 0]);
//! let Ok(Message::Midi2(address, Voice2::NoteOn { note, velocity, .. })) = decode(packet) else {
//!     unreachable!()
//! };
//! assert_eq!((address.channel.get(), note.get(), velocity), (0, 60, 0x8000));
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
//! The crate depends on no other tutti crate and has no allocation, so the
//! audio graph can carry packets without linking a MIDI stack.

#![forbid(unsafe_code)]

mod cc_number;
mod decode;
mod encode;
mod field;
mod message;
mod midi1;
mod midi_channel;
mod midi_group;
mod ump;

pub use cc_number::CCNumber;
pub use decode::decode;
pub use field::{
    DeltaTicks, FirstGroup, FunctionBlockId, GroupSpan, NoteNumber, OutOfRange, Pitch, SharpsFlats,
    U4, U7, U14,
};
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
pub use midi1::{Framed, Midi1Bytes, Midi1Parser, frame};
pub use ump::Ump;
