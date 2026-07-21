//! Hardware and file MIDI I/O for the Tutti engine.
//!
//! The crate has two worlds, kept in separate module trees:
//! - [`core`] — framework-free hardware I/O: [`MidiIo`], the [`MidiPort`] seam,
//!   the `midir`/`coremidi` driver edge, and the audio-thread ring buffers. Usable
//!   without Bevy.
//! - [`ecs`] — the `feature = "bevy"` ECS integration (components, systems,
//!   plugins) that wires the core into a Bevy app.
//!
//! Plus [`smf`], the Standard MIDI File codec, and passthrough re-exports of the
//! pure MIDI vocabulary from [`tutti_midi_types`]. The whole surface re-exports at
//! the crate root, so consumers write `tutti_midi_io::MidiIo` /
//! `tutti_midi_io::TuttiMidiPlugin` regardless of which world a type lives in.

// --- Framework-free hardware I/O core ---

pub mod core;
pub use core::error;
pub use core::{Error, Result};
pub use core::{MidiDevice, MidiInputRecord, MidiIo};
pub use core::{InputProducerHandle, MidiPortManager, PortInfo, PortType};

/// The protocol-transparent hardware seam: a [`MidiPort`] speaks [`MidiEvent`]
/// both ways and hides whether the wire is MIDI 1.0 or 2.0. See [`core::midi_port`]
/// for how a future UMP-native backend slots in behind the same trait.
pub use core::{MidiPort, SendError};

/// `MidiPortManager` and friends live in [`core::port`]; kept as a crate-root
/// module path for the `tutti_midi_io::port::*` spelling consumers already use.
pub use core::port;

#[cfg(all(target_os = "macos", feature = "virtual-midi"))]
pub use core::{VirtualMidiDestination, VirtualMidiSource};

// --- Re-exports from tutti-midi-types (the pure MIDI vocabulary) ---

pub use tutti_midi_types::Protocol;

pub use tutti_midi_types::{
    midi2, midly, normalize, MidiEvent, MidiMessage, MidiTarget, NoteAttribute, NoteId,
    PerNoteController, UnencodableMessage,
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
    read_clip_file, write_clip_file, write_clip_file_from_beats, ClipEvent, ClipFileError,
    ParsedClipFile,
};

#[cfg(feature = "mpe")]
pub use tutti_midi_types::mpe::{MpeMode, MpeZone, MpeZoneConfig};

pub use tutti_midi_types::cc::mapping::{CCMapping, CCNumber, CCTarget, MappingId, MidiChannel};

pub use tutti_midi_types::sync::{ClockTransportState, MidiClockDecoder, MtcDecoder, SmpteTimecode};

pub use crossbeam_channel;

// --- Standard MIDI File codec ---

/// Standard MIDI File (SMF) read/write — parse a `.mid` into beat-positioned
/// events ([`ParsedMidiFile`]) or per-track paired notes ([`smf::tracks`]), and
/// encode events back out ([`encode_midi_file`]).
pub mod smf;
pub use smf::{
    encode_midi_file, write_midi_file, MidiWriteOptions, ParsedMidiFile, SmfMessage, SmfNote,
    SmfTimedEvent, SmfTrack,
};

// --- Bevy ECS integration ---

#[cfg(feature = "bevy")]
pub mod ecs;

#[cfg(feature = "bevy")]
pub use ecs::{
    midi_input_event_system, midi_routing_sync_system, midi_sequence_setup_system,
    midi_sequence_tick_system, tick_scheduled_midi, MidiBusRes, MidiInputEvent, MidiInputObserver,
    MidiInputPlugin, MidiInputTranslators, MidiRoutingPlugin, MidiSequence, MidiSequenceNote,
    MidiSequencePlugin, MidiSequenceState, MidiSink, MidiSynthMarker, PendingMidi, ScheduledMidi,
    ScheduledMidiPlugin, TuttiMidiPlugin,
};

#[cfg(all(feature = "bevy", feature = "mpe"))]
pub use ecs::{MpeExpressionResource, MpeModeConfig, MpePlugin, MpeReceiver};

#[cfg(all(feature = "bevy", feature = "midi-hardware"))]
pub use ecs::{
    midi_device_connect_system, midi_device_poll_system, ConnectMidiDevice, DisconnectMidiDevice,
    MidiDeviceEvent, MidiDevicePlugin, MidiDeviceState, MidiIoRes,
};
