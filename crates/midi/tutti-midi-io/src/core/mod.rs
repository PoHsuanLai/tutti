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

pub mod error;
#[cfg(feature = "midi-hardware")]
pub mod midi_io;

pub(crate) mod hardware;
pub mod port;

pub use error::{Error, Result};
#[cfg(feature = "midi-hardware")]
pub use hardware::{MidiDevice, MidiInputRecord};
#[cfg(feature = "midi-hardware")]
pub use midi_io::MidiIo;
pub use port::{HardwareMidiInputs, InputProducerHandle, PortInfo, PortType};

#[cfg(all(target_os = "macos", feature = "midi-hardware"))]
pub use hardware::{UmpVirtualSource, VirtualMidiDestination, VirtualMidiSource};
