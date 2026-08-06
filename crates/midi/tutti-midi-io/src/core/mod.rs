//! Framework-free MIDI I/O — the engine core of this crate, usable without Bevy.
//!
//! - [`error`] — the crate's `Error` / `Result`.
//! - [`session`] — [`MidiSession`], the orchestrator: what is connected, and the
//!   sink to reach it. Owns no driver code, so it has no `#[cfg]`.
//! - [`backend`] — the per-OS [`MidiEndpoints`] implementations (CoreMIDI, ALSA
//!   seq-UMP, and a stub), plus `backend::active()` — the **only**
//!   `cfg(target_os)` in the crate that decides anything.
//! - [`endpoints`] — the backend seam itself. `open_output` yields a
//!   `Box<dyn MidiOut>`, so callers never learn which OS produced it.
//! - [`capability`] — what an endpoint is ([`EndpointInfo`]) and what it can
//!   carry ([`UmpCapability`]). A value, not a `cfg`, because two devices behind
//!   one backend can differ.
//! - [`port`] — the audio-thread ring-buffer plumbing ([`HardwareMidiInputs`] and
//!   its lock-free SPSC rings) that carries events between a driver callback and
//!   the audio graph.
//! - [`sysex`] — MIDI 1.0 SysEx reassembly and its promotion to UMP SysEx7.
//!   OS-free, so every driver edge shares it and it is testable without one.

pub mod backend;
pub mod capability;
pub mod endpoints;
pub mod error;
pub mod port;
pub mod session;
pub mod sysex;

pub use capability::{EndpointId, EndpointInfo, UmpCapability};
pub use endpoints::{InputConnection, MidiEndpoints};
pub use error::{Error, Result};
pub use port::{HardwareMidiInputs, InputProducerHandle, PortInfo, PortType};
pub use session::MidiSession;
pub use sysex::Sysex7Assembler;

#[cfg(all(target_os = "macos", feature = "midi-hardware"))]
pub use backend::coremidi::{UmpVirtualDestination, UmpVirtualSource};
