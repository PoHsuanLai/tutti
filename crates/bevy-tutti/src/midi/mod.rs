//! ECS integration for the MIDI subsystem.
//!
//! Grouped by function: each duty owns its components, systems, resources and a
//! focused sub-plugin in its own module, with [`TuttiMidiPlugin`] (in
//! [`plugin`]) as the composition root. Each resource lives with the duty that
//! owns it — [`MidiBusRes`] in [`bus`], [`MidiRoutingRes`] in [`routing`],
//! `MidiIoRes` in `device` (behind `midi-hardware`).
//!
//! The MIDI engine these systems drive, `tutti_midi_io`, is framework-free.

pub mod bus;
pub mod clock_out;
pub mod metadata;
pub mod negotiation;
pub mod routing;
pub mod scheduled;
pub mod sequence;
pub mod track_out;

#[cfg(feature = "midi-hardware")]
pub mod device;

pub mod mpe;

pub mod plugin;

pub use bus::MidiBusRes;
pub use clock_out::{pump_clock_out_system, ClockMasterRes, ClockOutPlugin};
#[cfg(all(target_os = "macos", feature = "midi-hardware"))]
pub use metadata::UmpOutRes;
pub use metadata::{
    flex_metadata_broadcast_system, BroadcastFlexMetadata, JrStamperRes, MidiMetadataPlugin,
};
pub use plugin::TuttiMidiPlugin;
pub use negotiation::{
    ci_discovery_system, ci_ingest_system, endpoint_discovery_system, endpoint_ingest_system,
    CiDeviceDiscovered, CiRes, EndpointDiscovered, EndpointDiscoveryRes, InboundCiMessage,
    InboundEndpointReply, MidiNegotiationPlugin, StartCiDiscovery, StartEndpointDiscovery,
};
pub use routing::{midi_routing_sync_system, MidiRoutingPlugin, MidiRoutingRes, MidiSink};
pub use scheduled::{tick_scheduled_midi, MidiSynthMarker, ScheduledMidi, ScheduledMidiPlugin};
pub use sequence::{
    midi_sequence_setup_system, midi_sequence_tick_system, MidiSequence, MidiSequenceNote,
    MidiSequencePlugin, MidiSequenceState,
};
pub use track_out::{
    midi_out_send_system, pump_midi_out_system, MidiOutPlugin, MidiOutRes, SendMidiOut,
};

pub use mpe::MpeModeConfig;
pub use routing::MpeReceiver;

#[cfg(feature = "midi-hardware")]
pub use device::{
    midi_device_connect_system, midi_device_poll_system, ConnectMidiDevice, DisconnectMidiDevice,
    MidiDeviceEvent, MidiDevicePlugin, MidiDeviceState, MidiIoRes,
};
