#![doc = include_str!("../README.md")]
// The whole crate is the ALSA UMP backend's FFI: `build.rs` sets `alsa_ump` on
// Linux when alsa-lib is 1.2.10 or newer, and otherwise there is nothing to link.
#![cfg(all(target_os = "linux", alsa_ump))]
#![cfg_attr(docsrs, feature(doc_cfg))]
// Every `unsafe` block says why it is sound, and every `unsafe fn` what it
// asks of its caller.
#![deny(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]
#![warn(unsafe_op_in_unsafe_fn)]

mod client;
mod error;
mod event;
mod poll;
mod ports;
mod records;
mod sys;

pub use client::{Client, QueueClock, Read};
pub use error::{Error, Result};
pub use event::{Announced, Received};
pub use poll::{Poller, Wait};
pub use ports::{PortEntry, Ports};

/// The port may be read from (`SND_SEQ_PORT_CAP_READ`).
pub const CAP_READ: u32 = sys::SND_SEQ_PORT_CAP_READ;
/// The port may be written to (`SND_SEQ_PORT_CAP_WRITE`).
pub const CAP_WRITE: u32 = sys::SND_SEQ_PORT_CAP_WRITE;
/// Other clients may subscribe to read from the port (`SND_SEQ_PORT_CAP_SUBS_READ`).
pub const CAP_SUBS_READ: u32 = sys::SND_SEQ_PORT_CAP_SUBS_READ;
/// Other clients may subscribe to write to the port (`SND_SEQ_PORT_CAP_SUBS_WRITE`).
pub const CAP_SUBS_WRITE: u32 = sys::SND_SEQ_PORT_CAP_SUBS_WRITE;
/// A generic MIDI port (`SND_SEQ_PORT_TYPE_MIDI_GENERIC`).
pub const TYPE_MIDI_GENERIC: u32 = sys::SND_SEQ_PORT_TYPE_MIDI_GENERIC;
/// A port owned by an application (`SND_SEQ_PORT_TYPE_APPLICATION`).
pub const TYPE_APPLICATION: u32 = sys::SND_SEQ_PORT_TYPE_APPLICATION;
