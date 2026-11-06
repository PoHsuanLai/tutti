//! The Universal MIDI Packet (M2-104): its storage, its addresses and its
//! messages.
//!
//! [`Ump`] is one packet as four `u32` words. [`MidiGroup`], [`MidiChannel`]
//! and [`CCNumber`] are the addresses a packet carries, as newtypes so that a
//! transposed `(group, channel)` pair does not compile. [`Message`] is one
//! defined message as a `Copy` value with no lifetime, and [`decode()`](fn@decode) and
//! `From<Message> for Ump` are the only code that knows a bit position.
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
//! Decoding reads no reserved bit, and a packet no definition covers comes back
//! as [`Unknown`] with its words, so a message from a later revision can be
//! forwarded unchanged. A MIDI 1.0 Note On at velocity 0 is a Note On here;
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
mod midi_channel;
mod midi_group;
mod ump;

pub use cc_number::CCNumber;
pub use decode::decode;
pub use field::{
    DeltaTicks, FunctionBlockCount, FunctionBlockId, GroupSpan, NoteNumber, SharpsFlats, U4, U7,
    U14,
};
pub use message::{
    Accidental, Address, Alteration, AlterationKind, Attribute, Bank, BlockFilter, BlockSelect,
    ChordBass, ChordName, ChordPitch, ChordType, Data128, DeviceIdentity, Direction,
    DiscoveryFilter, EndpointInfo, Flex, FlexAddress, FlexTextKind, Form, FunctionBlockInfo,
    KeySignature, Letter, Message, Metronome, Midi1Port, MixedDataHeader, Protocol, Stream,
    StreamConfig, Sysex7Packet, Sysex8Packet, SysexStatus, System, TimeSignature, UiHint,
    UmpVersion, Unknown, Utility, Voice1, Voice2,
};
pub use midi_channel::MidiChannel;
pub use midi_group::MidiGroup;
pub use ump::Ump;
