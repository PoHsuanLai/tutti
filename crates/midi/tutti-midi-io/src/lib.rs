pub mod error;
pub use error::{Error, Result};

mod midi_io;
pub use midi_io::MidiIo;
pub use hardware::{MidiDevice, MidiInputRecord};

// --- Re-exports from tutti-midi ---

pub use tutti_midi_types::{
    midi2, midly, normalize, MidiEvent, MidiMessage, MidiTarget, PerNoteController,
};

/// Stateful MIDI 1.0 → 2.0 translation (RPN/NRPN reassembly). Feed inbound CV1
/// events through [`Midi1ToMidi2Translator`] when a hardware source needs
/// multi-message (N)RPN runs collapsed into single MIDI-2 controller messages;
/// [`normalize`] alone handles only the stateless per-message quirks.
pub use tutti_midi_types::Midi1ToMidi2Translator;

/// MIDI 2.0 Clip File (M2-116) interchange — a portable single-clip UMP stream,
/// distinct from project save (Loro) and from SMF. See [`crate::smf`] for the
/// MIDI 1.0 equivalent.
pub use tutti_midi_types::{
    read_clip_file, write_clip_file, ClipEvent, ClipFileError, ParsedClipFile,
};

#[cfg(feature = "mpe")]
pub use tutti_midi_types::mpe::{MpeMode, MpeZone, MpeZoneConfig};

// --- Port management (ring-buffer I/O between hardware and audio thread) ---

pub mod port;
pub use port::{InputProducerHandle, MidiPortManager, PortInfo, PortType};

pub use tutti_midi_types::cc::mapping::{CCMapping, CCNumber, CCTarget, MappingId, MidiChannel};

// --- Local modules ---

pub(crate) mod hardware;

#[cfg(all(target_os = "macos", feature = "virtual-midi"))]
pub use hardware::{VirtualMidiDestination, VirtualMidiSource};

/// Standard MIDI File (SMF) read/write — parse a `.mid` into beat-positioned
/// events ([`ParsedMidiFile`]) or per-track paired notes ([`smf::tracks`]), and
/// encode events back out ([`encode_midi_file`]).
pub mod smf;
pub use smf::{
    encode_midi_file, write_midi_file, MidiWriteOptions, ParsedMidiFile, SmfNote, SmfTimedEvent,
    SmfTrack,
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

#[cfg(feature = "bevy")]
pub mod bus;
#[cfg(feature = "bevy")]
pub use bus::MidiBusRes;

#[cfg(feature = "bevy")]
pub mod input;
#[cfg(feature = "bevy")]
pub use input::{
    midi_input_event_system, MidiInputEvent, MidiInputObserver, MidiInputPlugin,
    MidiInputTranslators,
};

#[cfg(feature = "bevy")]
pub mod routing;
#[cfg(feature = "bevy")]
pub use routing::{midi_routing_sync_system, MidiReceiver, MidiRoutingPlugin};
#[cfg(all(feature = "bevy", feature = "mpe"))]
pub use routing::MpeReceiver;

#[cfg(feature = "bevy")]
pub mod sequence;
#[cfg(feature = "bevy")]
pub use sequence::{
    midi_sequence_setup_system, midi_sequence_tick_system, MidiSequence, MidiSequenceNote,
    MidiSequencePlugin, MidiSequenceState,
};

#[cfg(feature = "bevy")]
pub mod scheduled;
#[cfg(feature = "bevy")]
pub use scheduled::{tick_scheduled_midi, MidiSynthMarker, ScheduledMidi, ScheduledMidiPlugin};

#[cfg(all(feature = "bevy", feature = "midi-hardware"))]
pub mod device;
#[cfg(all(feature = "bevy", feature = "midi-hardware"))]
pub use device::{
    midi_device_connect_system, midi_device_poll_system, ConnectMidiDevice, DisconnectMidiDevice,
    MidiDeviceEvent, MidiDevicePlugin, MidiDeviceState, MidiIoRes,
};

#[cfg(all(feature = "bevy", feature = "mpe"))]
pub mod mpe;
#[cfg(all(feature = "bevy", feature = "mpe"))]
pub use mpe::{MpeExpressionResource, MpeModeConfig, MpePlugin};

#[cfg(feature = "bevy")]
mod midi_plugin;
#[cfg(feature = "bevy")]
pub use midi_plugin::{PendingMidi, TuttiMidiPlugin};
