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

/// Bevy ECS integration: MIDI-domain components, events, systems, and the
/// [`TuttiMidiPlugin`](ecs::TuttiMidiPlugin).
pub mod ecs;
// Re-export the ECS surface at the crate root so consumers write
// `tutti_midi_io::TuttiMidiPlugin`, not `tutti_midi_io::ecs::…` (the bevy_text shape).
pub use ecs::{
    midi_input_event_system, midi_routing_sync_system, midi_sequence_setup_system,
    midi_sequence_tick_system, tick_scheduled_midi, MidiBusRes, MidiInputEvent, MidiInputObserver,
    MidiReceiver, MidiSequence, MidiSequenceNote, MidiSequenceState, MidiSynthMarker, ScheduledMidi,
    TuttiMidiPlugin,
};
#[cfg(feature = "midi-hardware")]
pub use ecs::{
    midi_device_connect_system, midi_device_poll_system, ConnectMidiDevice, DisconnectMidiDevice,
    MidiDeviceEvent, MidiDeviceState, MidiIoRes,
};
#[cfg(feature = "mpe")]
pub use ecs::{MpeExpressionResource, MpeModeConfig, MpeReceiver};
