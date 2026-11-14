#![doc = include_str!("../README.md")]
//! The unsafe half of the Windows MIDI backend. Every `unsafe` call
//! `tutti-midi-hardware` makes into winmm lives here, behind safe handles; the
//! backend above it is `#![forbid(unsafe_code)]` and holds the logic
//! (enumeration identity, byte to UMP packing, timestamp placement).
//!
//! The crate is empty off Windows.
//!
//! ## What is here
//!
//! | | |
//! |---|---|
//! | `input_count`, `output_count`, `input_caps`, `output_caps` | the ports winmm lists now, by position, and what the driver reports of each |
//! | `MidiIn` | an open input: short messages and SysEx buffers delivered to a closure on the driver's thread, each with winmm's millisecond stamp; closing it returns every buffer first |
//! | `input_interface`, `output_interface` | the device interface path a driver gives a port, which names the device across a replug |
//! | `MidiOut` | an open output: a short message, or a SysEx sent whole |
//! | `Sleeper` | a high-resolution waitable timer and a wake signal: one wait that ends at a time or when woken |
//!
//! Each `unsafe` block carries one `// SAFETY:` argument where it is introduced;
//! `undocumented_unsafe_blocks` and `missing_safety_doc` are denied.
#![cfg(windows)]
#![cfg_attr(docsrs, feature(doc_cfg))]
// Every `unsafe` block says why it is sound, and every `unsafe fn` what it
// asks of its caller.
#![deny(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]
#![warn(unsafe_op_in_unsafe_fn)]

mod caps;
mod error;
mod input;
mod output;
mod sleeper;

pub use caps::{
    Caps, input_caps, input_count, input_interface, output_caps, output_count, output_interface,
};
pub use error::Error;
pub use input::{Event, MidiIn};
pub use output::MidiOut;
pub use sleeper::{Sleeper, Woken};
