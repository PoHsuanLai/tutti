pub mod error;
pub use error::{Error, Result};

mod midi_io;
pub use midi_io::{MidiDevice, MidiIo};
pub use io::MidiInputRecord;

// --- Re-exports from tutti-midi ---

pub use tutti_midi_types::{decode, midi2, midly, MidiEvent, MidiTarget, SemanticEvent};

#[cfg(feature = "mpe")]
pub use tutti_midi_types::mpe::{MpeMode, MpeZone, MpeZoneConfig};

// --- Port management (ring-buffer I/O between hardware and audio thread) ---

pub mod port;
pub use port::{InputProducerHandle, MidiPortManager, PortInfo, PortType};

pub use tutti_midi_types::cc::mapping::{CCMapping, CCNumber, CCTarget, MappingId, MidiChannel};

// --- Local modules ---

pub(crate) mod io;

#[cfg(all(target_os = "macos", feature = "virtual-midi"))]
pub use io::{VirtualMidiDestination, VirtualMidiSource};

pub(crate) mod file;
pub use file::{
    encode_midi_file, write_midi_file, MidiWriteOptions, ParsedMidiFile, SmfTimedEvent,
};

pub use tutti_midi_types::sync::{ClockTransportState, MidiClockDecoder, MtcDecoder, SmpteTimecode};

pub use crossbeam_channel;

// --- Bevy ECS integration ---
//
// The MIDI subsystem is grouped by FUNCTION: each duty (bus / input / routing /
// sequence / scheduled dispatch / hardware device / MPE) owns its components,
// systems, resources, and a focused sub-plugin in its own module.
// `TuttiMidiPlugin` (`midi_plugin.rs`) is the composition root that claims the
// engine handles and adds the sub-plugins. Each resource lives with the duty
// that owns it: `MidiBusRes` in `bus`, `MidiIoRes` in `device`, the transient
// `PendingMidi` next to its claimant in `midi_plugin`. The whole surface
// re-exports at the crate root so consumers write `tutti_midi_io::TuttiMidiPlugin`.

pub mod bus;
pub use bus::MidiBusRes;

pub mod input;
pub use input::{midi_input_event_system, MidiInputEvent, MidiInputObserver, MidiInputPlugin};

pub mod routing;
pub use routing::{midi_routing_sync_system, MidiReceiver, MidiRoutingPlugin};
#[cfg(feature = "mpe")]
pub use routing::MpeReceiver;

pub mod sequence;
pub use sequence::{
    midi_sequence_setup_system, midi_sequence_tick_system, MidiSequence, MidiSequenceNote,
    MidiSequencePlugin, MidiSequenceState,
};

pub mod scheduled;
pub use scheduled::{tick_scheduled_midi, MidiSynthMarker, ScheduledMidi, ScheduledMidiPlugin};

#[cfg(feature = "midi-hardware")]
pub mod device;
#[cfg(feature = "midi-hardware")]
pub use device::{
    midi_device_connect_system, midi_device_poll_system, ConnectMidiDevice, DisconnectMidiDevice,
    MidiDeviceEvent, MidiDevicePlugin, MidiDeviceState, MidiIoRes,
};

#[cfg(feature = "mpe")]
pub mod mpe;
#[cfg(feature = "mpe")]
pub use mpe::{MpeExpressionResource, MpeModeConfig, MpePlugin};

mod midi_plugin;
pub use midi_plugin::{PendingMidi, TuttiMidiPlugin};
