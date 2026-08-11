//! Rejoining multi-packet SysEx runs that arrive as UMP.
//!
//! A SysEx payload larger than one packet is fragmented at the source, and the
//! two types here put it back together — [`Sysex7PacketReassembler`] for
//! 7-bit runs (UMP type 0x3, keyed by group) and [`Sysex8PacketReassembler`] for
//! 8-bit ones (type 0x5, keyed by stream id).
//!
//! # The third one, and why it is not here
//!
//! `tutti-midi-hardware` has a `Sysex7ByteAssembler`, and the three are easy to
//! confuse: `Assembler` versus `Reassembler` is not a distinction anyone can
//! infer. Read them by **what they consume**, which is the thing that actually
//! differs:
//!
//! | type | consumes | produces |
//! |---|---|---|
//! | `Sysex7ByteAssembler` | raw `F0 … F7` **bytes** | UMP packets |
//! | [`Sysex7PacketReassembler`] | UMP **packets** | the complete run |
//! | [`Sysex8PacketReassembler`] | UMP **packets** | the decoded payload |
//!
//! They are adjacent stages of one pipeline, not competing implementations. The
//! byte assembler stays in the hardware crate because only a driver produces
//! loose bytes — a UMP-native transport hands over packets, already fragmented.

pub mod reassembler7;
pub mod reassembler8;

pub use reassembler7::{Sysex7PacketReassembler, DEFAULT_MAX_SYSEX_BYTES};
pub use reassembler8::{
    Sysex8Abort, Sysex8Event, Sysex8PacketReassembler, DEFAULT_MAX_SYSEX8_BYTES,
};
