//! Hardware and file MIDI I/O for the Tutti engine.
//!
//! The crate has two worlds, kept in separate module trees:
//! - [`core`] — framework-free hardware I/O: [`MidiIo`], the `midir`/`coremidi`
//!   driver edge, and the audio-thread ring buffers. Usable without Bevy.
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
// OS hardware orchestrator, the device descriptor, and the record its observer
// channel carries — only present under `midi-hardware` (they own the `midir`
// edge).
pub use core::{HardwareMidiInputs, InputProducerHandle, PortInfo, PortType};
#[cfg(feature = "midi-hardware")]
pub use core::{MidiDevice, MidiInputRecord, MidiIo};

/// `HardwareMidiInputs` and friends live in [`core::port`]; kept as a crate-root
/// module path for the `tutti_midi_io::port::*` spelling consumers already use.
pub use core::port;

#[cfg(all(target_os = "macos", feature = "midi-hardware"))]
pub use core::{UmpVirtualSource, VirtualMidiDestination, VirtualMidiSource};

// --- Re-exports from tutti-midi-types (the pure MIDI vocabulary) ---

pub use tutti_midi_types::Protocol;

pub use tutti_midi_types::{
    midi2, midly, normalize, MidiEvent, MidiIn, MidiMessage, MidiOut, MidiUnitId, NoteAttribute,
    NoteId, PerNoteController, UmpMessageType, UnencodableMessage,
};

/// MIDI-CI (M2-101) message codec + SysEx7 wire bridge. Re-exported so the app's
/// inbound-decode path can turn a reassembled SysEx7 run into a `CiMessage`
/// (`ci::sysex7_to_ci`) to feed the negotiators.
pub use tutti_midi_types::ci;

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

pub use tutti_midi_types::mpe::{MpeMode, MpeZone, MpeZoneConfig};

pub use tutti_midi_types::cc::mapping::{CCMapping, CCNumber, CCTarget, MappingId, MidiChannel};

pub use tutti_midi_types::sync::{
    ClockTransportState, MidiClockDecoder, MtcDecoder, SmpteFrameRate, SmpteTimecode,
};

// --- Runtime delivery (event fan-out + beat-scheduled playback) ---
//
// The lock-free dispatch (`MidiBus`/`MidiSender`/`MidiReceiver`) and the offline
// snapshot / clip playback live in `tutti-midi-runtime`; surface them here so an
// app depends on this one umbrella crate rather than reaching into the runtime.

pub use tutti_midi_runtime::{
    MidiBus, MidiClipSource, MidiMailbox, MidiReceiver, MidiSender, MidiSnapshot,
    Sysex7Reassembler, TimedClipEvent, TimedMidiEvent,
};

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

/// The umbrella MIDI prelude, for `use tutti_midi_io::prelude::*;` — everything a
/// typical app touches, from one import.
///
/// It re-exports [`tutti_midi_types::prelude`] (the wire event + decoded view +
/// clip-file codec + per-note identity) and adds this crate's I/O and delivery:
///
/// - **Hardware I/O** — [`MidiIo`] (connect / send / observe) and [`MidiDevice`].
/// - **Delivery** — [`MidiBus`] / [`MidiSender`] / [`MidiReceiver`] (lock-free
///   fan-out), and beat-scheduled playback ([`MidiClipSource`], [`MidiSnapshot`],
///   [`TimedMidiEvent`]).
///
/// Deliberately excludes the rarer surfaces — SMF codec internals, UMP-Stream
/// endpoint negotiation, the Bevy ECS layer, sync decoders, MPE zone config —
/// which stay explicit imports (`tutti_midi_io::smf`, `::MidiClockDecoder`,
/// `::ecs::*`, …). Glob this for the 90% path; import the rest by name.
///
/// ```
/// use tutti_midi_io::prelude::*;
///
/// // The types prelude comes along: build + decode an event.
/// let ev = MidiEvent::note_on(0, 0, 60, 0x8000);
/// assert!(ev.message().is_note_on());
///
/// // And the delivery types are here too — fan an event to a unit's inbox.
/// let (tx, rx) = MidiMailbox::pair(MidiUnitId::new(1));
/// let bus = MidiBus::new();
/// bus.insert(tx);
/// bus.queue(MidiUnitId::new(1), &[ev]);
/// let mut buf = [ev; 4];
/// assert_eq!(rx.poll_into(&mut buf), 1);
/// ```
pub mod prelude {
    pub use tutti_midi_types::prelude::*;

    pub use crate::{
        MidiBus, MidiClipSource, MidiMailbox, MidiReceiver, MidiSender, MidiSnapshot,
        TimedMidiEvent,
    };

    // The OS orchestrator + device descriptor only exist under `midi-hardware`.
    #[cfg(feature = "midi-hardware")]
    pub use crate::{MidiDevice, MidiIo};
}

// --- Bevy ECS integration ---

#[cfg(feature = "bevy")]
pub mod ecs;

#[cfg(feature = "bevy")]
pub use ecs::{
    midi_routing_sync_system, midi_sequence_setup_system, midi_sequence_tick_system,
    pump_clock_out_system, tick_scheduled_midi, ClockMasterRes, ClockOutPlugin, MidiBusRes,
    MidiRoutingPlugin, MidiRoutingRes, MidiSequence, MidiSequenceNote, MidiSequencePlugin,
    MidiSequenceState, MidiSink, MidiSynthMarker, PendingMidi, ScheduledMidi, ScheduledMidiPlugin,
    TuttiMidiPlugin,
};

#[cfg(feature = "bevy")]
pub use ecs::{
    BroadcastFlexMetadata, CiDeviceDiscovered, CiRes, EndpointDiscovered, EndpointDiscoveryRes,
    InboundCiMessage, InboundEndpointReply, JrStamperRes, MidiMetadataPlugin,
    MidiNegotiationPlugin, MidiOutPlugin, MidiOutRes, SendMidiOut, StartCiDiscovery,
    StartEndpointDiscovery,
};

#[cfg(all(feature = "bevy", target_os = "macos", feature = "midi-hardware"))]
pub use ecs::UmpOutRes;

#[cfg(feature = "bevy")]
pub use ecs::{MpeExpressionResource, MpeModeConfig, MpePlugin, MpeReceiver};

#[cfg(all(feature = "bevy", feature = "midi-hardware"))]
pub use ecs::{
    midi_device_connect_system, midi_device_poll_system, ConnectMidiDevice, DisconnectMidiDevice,
    MidiDeviceEvent, MidiDevicePlugin, MidiDeviceState, MidiIoRes,
};
