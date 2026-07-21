//! Framework-free hardware MIDI I/O — the engine core of this crate, usable
//! without Bevy.
//!
//! - [`error`] — the crate's `Error` / `Result`.
//! - [`midi_io`] — [`MidiIo`], the single orchestrator for hardware MIDI
//!   (connect/disconnect devices, send events, observe input).
//! - [`midi_port`] — the protocol-transparent [`MidiPort`] seam (`Midi1Port` over
//!   `midir`; a future UMP-native port slots in behind the same trait).
//! - `hardware` — the `midir`/`coremidi` driver edge (port enumeration,
//!   connections, the background send thread, macOS virtual ports).
//! - [`port`] — the audio-thread ring-buffer plumbing ([`MidiPortManager`] and
//!   its lock-free SPSC rings) that carries events between hardware and the
//!   audio graph.

pub mod error;
pub mod midi_io;
pub mod midi_port;

pub(crate) mod hardware;
pub mod port;

pub use error::{Error, Result};
pub use hardware::{MidiDevice, MidiInputRecord};
pub use midi_io::MidiIo;
pub use midi_port::{MidiPort, SendError};
pub use port::{InputProducerHandle, MidiPortManager, PortInfo, PortType};

#[cfg(all(target_os = "macos", feature = "virtual-midi"))]
pub use hardware::{VirtualMidiDestination, VirtualMidiSource};
