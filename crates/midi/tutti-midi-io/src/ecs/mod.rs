//! Bevy ECS integration for the MIDI subsystem (`feature = "bevy"`).
//!
//! Grouped by FUNCTION: each duty owns its components, systems, resources, and a
//! focused sub-plugin in its own module. [`TuttiMidiPlugin`] (in [`midi_plugin`])
//! is the composition root that claims the engine handles and adds the
//! sub-plugins. Each resource lives with the duty that owns it: [`MidiBusRes`] in
//! [`bus`], `MidiIoRes` in `device` (behind `midi-hardware`), the transient
//! [`PendingMidi`] next to its claimant in [`midi_plugin`]. The whole surface
//! re-exports at the crate root so consumers write `tutti_midi_io::TuttiMidiPlugin`.
//!
//! This is the only Bevy-dependent part of the crate; everything under
//! [`crate::core`] is framework-free.

pub mod bus;
pub mod clock_out;
pub mod metadata;
pub mod negotiation;
pub mod routing;
pub mod scheduled;
pub mod sequence;

#[cfg(feature = "midi-hardware")]
pub mod device;

pub mod mpe;

pub mod midi_plugin;

pub use bus::MidiBusRes;
pub use clock_out::{pump_clock_out_system, ClockMasterRes, ClockOutPlugin};
pub use metadata::{
    flex_metadata_broadcast_system, BroadcastFlexMetadata, JrStamperRes, MidiMetadataPlugin,
};
#[cfg(all(target_os = "macos", feature = "midi-hardware"))]
pub use metadata::UmpOutRes;
pub use midi_plugin::{PendingMidi, TuttiMidiPlugin};
pub use negotiation::{
    ci_discovery_system, ci_ingest_system, endpoint_discovery_system, endpoint_ingest_system,
    CiDeviceDiscovered, CiRes, EndpointDiscovered, EndpointDiscoveryRes, InboundCiMessage,
    InboundEndpointReply, MidiNegotiationPlugin, StartCiDiscovery, StartEndpointDiscovery,
};
pub use routing::{midi_routing_sync_system, MidiRoutingPlugin, MidiSink};
pub use scheduled::{tick_scheduled_midi, MidiSynthMarker, ScheduledMidi, ScheduledMidiPlugin};
pub use sequence::{
    midi_sequence_setup_system, midi_sequence_tick_system, MidiSequence, MidiSequenceNote,
    MidiSequencePlugin, MidiSequenceState,
};

pub use mpe::{MpeExpressionResource, MpeModeConfig, MpePlugin};
pub use routing::MpeReceiver;

#[cfg(feature = "midi-hardware")]
pub use device::{
    midi_device_connect_system, midi_device_poll_system, ConnectMidiDevice, DisconnectMidiDevice,
    MidiDeviceEvent, MidiDevicePlugin, MidiDeviceState, MidiIoRes,
};
