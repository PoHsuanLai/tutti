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
