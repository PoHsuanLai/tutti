//! Hardware MIDI driver layer — the midir/coremidi edge the [`MidiIo`]
//! orchestrator owns. `input` enumerates + opens input ports (and mints the
//! [`MidiInputRecord`]s the observer channel carries); `output` enumerates
//! output ports and runs the background send thread. [`MidiDevice`] is the
//! descriptor this layer speaks — it lives here, with the driver code that
//! produces it, rather than up at the orchestrator.
//!
//! [`MidiIo`]: crate::core::MidiIo

#[cfg(feature = "midi-hardware")]
mod input;
#[cfg(feature = "midi-hardware")]
mod output;

#[cfg(feature = "midi-hardware")]
pub(crate) use input::connect_midi_input;
#[cfg(feature = "midi-hardware")]
pub use input::{list_input_devices, MidiInputRecord};
#[cfg(feature = "midi-hardware")]
pub use output::list_output_devices;
#[cfg(feature = "midi-hardware")]
pub(crate) use output::{OutputCmd, OutputThread};

// The native-UMP virtual endpoints moved to `core::backend::coremidi`, beside
// the rest of the CoreMIDI code. Re-exported here only until `MidiIo` (this
// module's consumer) is replaced by `MidiSession`.
#[cfg(all(target_os = "macos", feature = "midi-hardware"))]
pub use crate::core::backend::coremidi::{UmpVirtualDestination, UmpVirtualSource};

/// A detected MIDI device (input or output).
#[cfg(feature = "midi-hardware")]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MidiDevice {
    pub index: usize,
    pub name: String,
}
