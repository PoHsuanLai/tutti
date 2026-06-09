//! Hardware MIDI device I/O — the midir-backed port layer the [`MidiIo`]
//! facade owns. `device_in` enumerates + opens input ports (and mints the
//! [`MidiInputRecord`]s the observer channel carries); `device_out` enumerates
//! output ports and runs the background send thread.
//!
//! [`MidiIo`]: crate::MidiIo

mod device_in;
mod device_out;

#[cfg(all(target_os = "macos", feature = "virtual-midi"))]
pub mod virtual_port;

pub(crate) use device_in::connect_midi_input;
pub use device_in::{list_input_devices, MidiInputRecord};
pub use device_out::list_output_devices;
pub(crate) use device_out::{OutputCmd, OutputThread};

#[cfg(all(target_os = "macos", feature = "virtual-midi"))]
pub use virtual_port::{VirtualMidiDestination, VirtualMidiSource};
