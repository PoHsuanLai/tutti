//! Universal MIDI Packet (M2-104) storage and addressing.
//!
//! [`Ump`] is one packet as four `u32` words. [`MidiGroup`], [`MidiChannel`]
//! and [`CCNumber`] are the addresses a packet carries, as newtypes so that a
//! transposed `(group, channel)` pair does not compile.
//!
//! The crate depends on no other tutti crate, so the audio graph can carry
//! packets without linking a MIDI codec. `serde` (off by default) derives
//! `Serialize` and `Deserialize` on the three address types.

#![forbid(unsafe_code)]

mod cc_number;
mod midi_channel;
mod midi_group;
mod ump;

pub use cc_number::CCNumber;
pub use midi_channel::MidiChannel;
pub use midi_group::MidiGroup;
pub use ump::Ump;
