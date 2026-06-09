//! Hardware MIDI driver layer — the midir/coremidi edge the [`MidiIo`]
//! orchestrator owns. `input` enumerates + opens input ports (and mints the
//! [`MidiInputRecord`]s the observer channel carries); `output` enumerates
//! output ports and runs the background send thread. [`MidiDevice`] is the
//! descriptor this layer speaks — it lives here, with the driver code that
//! produces it, rather than up at the orchestrator.
//!
//! [`MidiIo`]: crate::MidiIo

mod input;
mod output;

#[cfg(all(target_os = "macos", feature = "virtual-midi"))]
pub mod virtual_port;

pub(crate) use input::connect_midi_input;
pub use input::{list_input_devices, MidiInputRecord};
pub use output::list_output_devices;
pub(crate) use output::{OutputCmd, OutputThread};

#[cfg(all(target_os = "macos", feature = "virtual-midi"))]
pub use virtual_port::{VirtualMidiDestination, VirtualMidiSource};

/// A detected MIDI device (input or output).
#[derive(Debug, Clone)]
pub struct MidiDevice {
    pub index: usize,
    pub name: String,
}
