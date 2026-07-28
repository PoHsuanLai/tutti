//! ECS integration for the MIDI subsystem.
//!
//! One duty per module, each owning its components, systems and resources, with
//! [`TuttiMidiPlugin`] (in [`plugin`]) as the composition root. The MIDI engine
//! these systems drive is framework-free; this layer only wraps it.
//!
//! # Addressing a synth
//!
//! A MIDI-receiving node owns a [`MidiInPort`](tutti_midi_runtime::MidiInPort):
//! its routing address, its push mailbox, and the slot a beat-scheduled source
//! installs into. The ECS layer never stores that address — it resolves it from
//! the graph each time, because a `crossfade` can replace a node's unit while
//! keeping its `NodeId`, leaving any stored copy silently stale. See
//! [`target`] for the registry a host fills in, and [`registration`] for how a
//! node's sender gets on (and off) the bus.
//!
//! ```rust,ignore
//! app.world_mut()
//!     .resource_mut::<MidiTargetRegistry>()
//!     .register::<tutti_synth::SoundFontUnit>();
//! ```

pub mod bus;
pub mod clock_out;
pub mod metadata;
pub mod negotiation;
pub mod registration;
pub mod routing_table;
pub mod sequence;
pub mod target;
pub mod track_out;

#[cfg(feature = "midi-hardware")]
pub mod device;

pub mod plugin;

/// Constructors an integration test needs and production code must not have.
///
/// [`MidiBusRes`] has no public constructor on purpose — it must be the instance
/// the RT pre-block shares, and letting a host build one is the silent-failure
/// mode the type exists to prevent. But a test that wants to prove registration
/// works has no audio device and no pre-block, so it needs *some* way in.
///
/// Gated on `cfg(test)`-equivalent visibility would not reach an integration
/// test (a separate crate), hence a module rather than `#[cfg(test)]`. It is
/// documented as test-only and named to make a production call site look wrong
/// in review.
pub mod test_support {
    use super::MidiBusRes;

    /// A standalone bus, wired to nothing. **Tests only** — a bus built here is
    /// not the one any audio thread reads.
    pub fn midi_bus_for_test() -> MidiBusRes {
        MidiBusRes::new(tutti_midi_runtime::MidiBus::new())
    }
}

pub use bus::{MidiBusRes, MpeModeConfig};
pub use clock_out::{pump_clock_out_system, ClockMasterRes, ClockOutPlugin};
#[cfg(all(target_os = "macos", feature = "midi-hardware"))]
pub use metadata::UmpOutRes;
pub use metadata::{
    flex_metadata_broadcast_system, BroadcastFlexMetadata, JrStamperRes, MidiMetadataPlugin,
};
pub use negotiation::{
    ci_discovery_system, ci_ingest_system, endpoint_discovery_system, endpoint_ingest_system,
    CiDeviceDiscovered, CiRes, EndpointDiscovered, EndpointDiscoveryRes, InboundCiMessage,
    InboundEndpointReply, MidiNegotiationPlugin, StartCiDiscovery, StartEndpointDiscovery,
};
pub use plugin::TuttiMidiPlugin;
pub use registration::{
    register_midi_senders, unregister_midi_sender, MidiRegistered, MidiRegistrationPlugin,
};
pub use routing_table::MidiRoutingRes;
pub use sequence::{
    rebuild as rebuild_midi_sources, InstalledMidiSources, MidiNote, MidiSequencePlugin,
    MidiSourceInstall,
};
pub use target::{MidiNode, MidiTargetRegistry, MidiTargetResolver};
pub use track_out::{
    midi_out_send_system, pump_midi_out_system, MidiOutPlugin, MidiOutRes, SendMidiOut,
};

#[cfg(feature = "midi-hardware")]
pub use device::{
    midi_device_connect_system, midi_device_poll_system, ConnectMidiDevice, DisconnectMidiDevice,
    MidiDeviceEvent, MidiDevicePlugin, MidiDeviceState, MidiIoRes,
};

