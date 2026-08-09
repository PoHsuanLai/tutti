//! ECS integration for the MIDI subsystem.
//!
//! One duty per module, each owning its components, systems and resources, with
//! [`TuttiMidiPlugin`] (in [`plugin`]) as the composition root. The MIDI engine
//! these systems drive is framework-free; this layer only wraps it.
//!
//! This module mirrors the engine's `midi/` *tier*, not a single crate — the
//! exception to the one-module-per-engine-crate shape, because the four crates
//! underneath don't each earn an adapter. `tutti-midi-types` is vocabulary and
//! needs none; [`file`] adapts `tutti-midi-file` (codecs, no OS port);
//! [`device`] adapts `tutti-midi-hardware` and is gated with it behind
//! `midi-hardware`; every other module here adapts `tutti-midi-runtime`.
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
//!     .register::<tutti_soundfont::SoundFontUnit>();
//! ```

pub mod bus;
pub mod clock_out;
pub mod hardware_out;
pub mod metadata;
pub mod negotiation;
pub mod out_sink;
pub mod registration;
pub mod route;
pub mod routing_table;
pub mod sequence;
pub mod target;
pub mod track_out;

#[cfg(feature = "midi-hardware")]
pub mod device;

/// MIDI file IO on the task pool.
pub mod file;

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

    /// A routing table and the snapshot handle the RT pre-block would hold.
    ///
    /// **Tests only.** Returns both halves because the whole point of the type
    /// is that they are one shared cell: a test asserts through the resource
    /// and reads back through the arc, which is the invariant `MidiRoutingRes`
    /// exists to protect.
    pub fn routing_table_for_test() -> (
        super::MidiRoutingRes,
        std::sync::Arc<
            tutti_core::RtPublish<tutti_midi_runtime::tutti_midi_types::MidiRoutingSnapshot>,
        >,
    ) {
        let table = tutti_midi_runtime::tutti_midi_types::MidiRoutingTable::new();
        let rt_view = table.snapshot_arc();
        (super::MidiRoutingRes::new(table), rt_view)
    }

    /// Everything sitting in the outbound MIDI-out mailbox, drained.
    ///
    /// **Tests only.** The production drain is
    /// [`pump_midi_out_system`](super::track_out::pump_midi_out_system), which
    /// routes each event to hardware and keeps none — so a test asserting *what
    /// a producer queued* has to read the mailbox itself, and an integration
    /// test cannot reach `MidiOutRes`'s crate-private receiver.
    pub fn drain_midi_out(
        out: &super::track_out::MidiOutRes,
    ) -> Vec<tutti_midi_runtime::tutti_midi_types::ump::MidiEvent> {
        out.drain_for_test()
    }

    /// A clock master wired to a fresh transport. **Tests only.**
    ///
    /// Systems gated on `engine_ready` take this as a plain `Res`, because the
    /// engine block always inserts it — so a test that claims the engine is
    /// running has to supply it or those systems panic on a missing resource.
    pub fn clock_master_for_test(sample_rate: f64) -> super::ClockMasterRes {
        let (sender, receiver) = tutti_midi_runtime::MidiMailbox::pair(
            tutti_midi_runtime::tutti_midi_types::MidiUnitId::next(),
        );
        let master = std::sync::Arc::new(tutti_midi_runtime::ClockMaster::new(
            std::sync::Arc::new(tutti_core::transport::Transport::new(sample_rate)),
            sample_rate,
            sender,
        ));
        super::ClockMasterRes::new(master, receiver)
    }
}

pub use bus::{MidiBusRes, MpeModeConfig, MpeModeHandle};
pub use clock_out::{pump_clock_out_system, ClockMasterRes, ClockOutPlugin};
pub use file::{
    MidiFileAsset, MidiFileAssetLoader, MidiFileContents, MidiFileLoaderError, MidiFilePlugin,
    MidiFileWrite, MidiFileWriteInFlight, MidiFileWritten,
};
#[cfg(all(target_os = "macos", feature = "midi-hardware"))]
pub use hardware_out::UmpOutRes;
pub use hardware_out::{drain_receiver_through, JrStamperRes, MidiOutDrops, MidiOutRouter};
pub use metadata::{flex_metadata_broadcast_system, BroadcastFlexMetadata, MidiMetadataPlugin};
pub use negotiation::{
    ci_discovery_system, ci_ingest_system, endpoint_discovery_system, endpoint_ingest_system,
    CiDeviceDiscovered, CiRes, EndpointDiscovered, EndpointDiscoveryRes, InboundCiMessage,
    InboundEndpointReply, MidiNegotiationPlugin, StartCiDiscovery, StartEndpointDiscovery,
};
pub use out_sink::MidiOutSinkRes;
pub use plugin::TuttiMidiPlugin;
pub use registration::{
    register_midi_senders, unregister_midi_sender, MidiRegistered, MidiRegistrationPlugin,
};
pub use route::{
    rebuild as rebuild_midi_routes, MidiRouteFallback, MidiRoutePlugin, MidiRouteRule,
};
pub use routing_table::MidiRoutingRes;
pub use sequence::{
    rebuild as rebuild_midi_sources, InstalledMidiSources, MidiSequencePlugin, MidiSourceInstall,
};
pub use target::{MidiNode, MidiTargetRegistry, MidiTargetResolver};
pub use track_out::{
    midi_out_send_system, pump_midi_out_system, MidiOutPlugin, MidiOutRes, SendMidiOut,
};

#[cfg(feature = "midi-hardware")]
pub use device::{
    midi_device_connect_system, midi_device_poll_system, ConnectMidiDevice, ConnectMidiOutput,
    DisconnectMidiDevice, DisconnectMidiOutput, MidiDeviceEvent, MidiDevicePlugin, MidiDeviceState,
    MidiDirection, MidiIoRes,
};
