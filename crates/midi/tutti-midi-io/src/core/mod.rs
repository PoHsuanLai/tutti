//! Framework-free hardware MIDI I/O — the engine core of this crate, usable
//! without Bevy.
//!
//! - [`error`] — the crate's `Error` / `Result`.
//! - [`midi_io`] — [`MidiIo`], the single orchestrator for hardware MIDI
//!   (connect/disconnect devices, send events, observe input).
//! - `hardware` — the `midir`/`coremidi` driver edge (port enumeration,
//!   connections, the background send thread, macOS virtual ports). `Midi1Port`
//!   translates engine [`MidiEvent`](tutti_midi_types::MidiEvent)s to MIDI 1.0
//!   wire bytes at its edge.
//! - [`port`] — the audio-thread ring-buffer plumbing ([`HardwareMidiInputs`] and
//!   its lock-free SPSC rings) that carries events between hardware and the
//!   audio graph.
//! - [`sysex`] — MIDI 1.0 SysEx reassembly and its promotion to UMP SysEx7.
//!   OS-free, so it is shared by every driver edge and testable without one.
//! - [`capability`] — what an endpoint is ([`EndpointInfo`]) and what it can
//!   carry ([`UmpCapability`]). A value, not a `cfg`, because two devices behind
//!   one backend can differ.
//! - [`endpoints`] — the [`MidiEndpoints`] backend seam: enumerate and open this
//!   OS's endpoints. Its `open_output` yields a `Box<dyn MidiOut>`, so callers
//!   never learn which OS produced it.

pub mod error;
#[cfg(feature = "midi-hardware")]
pub mod midi_io;

pub mod backend;
pub mod capability;
pub mod endpoints;
pub(crate) mod hardware;
pub mod port;
pub mod sysex;

pub use capability::{EndpointId, EndpointInfo, UmpCapability};
pub use endpoints::{InputConnection, MidiEndpoints};
pub use error::{Error, Result};
#[cfg(feature = "midi-hardware")]
pub use hardware::{MidiDevice, MidiInputRecord};
#[cfg(feature = "midi-hardware")]
pub use midi_io::MidiIo;
pub use port::{HardwareMidiInputs, InputProducerHandle, PortInfo, PortType};
pub use sysex::Sysex7Assembler;

#[cfg(all(target_os = "macos", feature = "midi-hardware"))]
pub use hardware::{UmpVirtualDestination, UmpVirtualSource};
