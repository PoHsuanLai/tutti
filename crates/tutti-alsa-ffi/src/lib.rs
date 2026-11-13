#![doc = include_str!("../README.md")]
//! The unsafe half of the Linux MIDI backend. Every `unsafe` call
//! `tutti-midi-hardware` makes into alsa-lib's sequencer and UMP API lives here,
//! behind safe handles; the backend above it is `#![forbid(unsafe_code)]` and holds
//! the logic (the pump and watch threads, the device model, the id packing).
//!
//! Linux only, and only where alsa-lib is 1.2.10 or newer (the build script probes
//! it and sets `cfg(alsa_ump)`); on other targets, or with an older or missing
//! alsa-lib, the crate is empty and the backend takes its stub.
//!
//! ## What is here
//!
//! | | |
//! |---|---|
//! | `Client` | an open sequencer client in UMP MIDI-2.0 mode: it closes on drop, and ports, subscriptions, `read_event`, `send_to_subscribers`, `stamp_port` and `list_ports` are methods |
//! | `UmpEndpoint`, `UmpBlock` | what the sequencer records of a client's UMP Endpoint (the protocol in use, the protocols offered) and its Function Blocks, from `Client::ump_endpoint` |
//! | `QueueClock` | the queue a stamped port reads its timestamps from, and the clock that says what time it is now |
//! | `Poller` | a client's descriptors and a stop descriptor, waited on with `poll(2)` |
//! | `Received`, `Announced` | an event copied out of alsa-lib's buffer, and what an announce-port event says |
//! | `Error` | the alsa-lib call that failed and its negative code |
//!
//! The raw declarations are a private module (`sys`), with alsa-lib's C names so a
//! reader can grep its headers. `snd_seq_ump_event_t` mirrors a C struct whose last
//! member is a union, so its size and field offsets are measured against the real
//! header and re-checked by compile-time asserts.
//!
//! Each `unsafe` block carries one `// SAFETY:` argument where it is introduced;
//! `undocumented_unsafe_blocks` and `missing_safety_doc` are denied.
//!
//! ## Linking and `links`
//!
//! `alsa-sys` (through `cpal`) declares `links = "alsa"`, so this crate declares
//! `links = "asound"` and the two coexist. Both link `libasound`, which is
//! harmless. The key also carries the build script's probe result to dependents as
//! `DEP_ASOUND_ALSA_UMP` (alsa-lib >= 1.2.10) and `DEP_ASOUND_ALSA_UMP_CREATE`
//! (>= 1.2.13). With an older or missing alsa-lib the build warns and sets neither;
//! nothing is linked.
//!
//! ## Features
//!
//! `testing` adds `Client::rename` and `Client::set_port_caps`, which let a
//! test stand in for a device that appears, changes and goes. Off in a production
//! build.
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
mod ump;

#[cfg(feature = "testing")]
pub use client::LegacyEvent;
pub use client::{Client, QueueClock, Read, UmpVersion};
pub use error::{Error, Result};
pub use event::{Announced, Received};
pub use poll::{Poller, Wait};
pub use ports::{PortEntry, Ports};
pub use ump::{
    BLOCK_IS_LOWSPEED, BLOCK_IS_MIDI1, DIR_BIDIRECTION, DIR_INPUT, DIR_OUTPUT, PROTO_JRTS_RX,
    PROTO_JRTS_TX, PROTO_MIDI1, PROTO_MIDI2, UmpBlock, UmpEndpoint,
};

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
