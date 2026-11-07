//! Rejoining multi-packet SysEx runs that arrive as UMP.
//!
//! A SysEx payload larger than one packet is fragmented at the source, and the
//! two types here put it back together — [`Sysex7PacketReassembler`] for
//! 7-bit runs (UMP type 0x3, keyed by group) and [`Sysex8PacketReassembler`] for
//! 8-bit ones (type 0x5, keyed by stream id).
//!
//! # Bytes
//!
//! A transport that delivers raw `F0 … F7` bytes is read by
//! `tutti_ump::Midi1Parser`, which produces the same SysEx7 packets one byte at
//! a time. These two types consume those packets:
//!
//! | type | consumes | produces |
//! |---|---|---|
//! | `Midi1Parser` | raw `F0 … F7` **bytes** | UMP packets |
//! | [`Sysex7PacketReassembler`] | UMP **packets** | the complete run |
//! | [`Sysex8PacketReassembler`] | UMP **packets** | the decoded payload |
//!
//! They are adjacent stages of one pipeline, not competing implementations.

pub mod reassembler7;
pub mod reassembler8;

pub use reassembler7::{DEFAULT_MAX_SYSEX_BYTES, Sysex7PacketReassembler};
pub use reassembler8::{
    DEFAULT_MAX_SYSEX8_BYTES, Sysex8Abort, Sysex8Event, Sysex8PacketReassembler,
};
