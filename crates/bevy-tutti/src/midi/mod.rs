//! ECS integration for the MIDI subsystem.
//!
//! One duty per module, each owning its components, systems and resources, with
//! [`TuttiMidiPlugin`] (in [`plugin`]) as the composition root. The MIDI engine
//! these systems drive is framework-free; this layer only wraps it.
//!
//! This module mirrors the engine's `midi/` *tier*, not a single crate — the
//! exception to the one-module-per-engine-crate shape, because the four crates
//! underneath don't each earn an adapter. `tutti-midi-types` is vocabulary and
//! needs none; [`file`](mod@self::file) adapts `tutti-midi-file` (codecs, no OS port);
//! [`hardware::device`] adapts `tutti-midi-hardware` and is gated with it
//! behind `midi-hardware`; everything else adapts `tutti-midi-runtime`.
//!
//! Within the tier, the modules group by duty, and each directory's rule is
//! one sentence:
//!
//! - [`endpoint`] — how a node becomes reachable, in both directions. No
//!   routing, no hardware, no time.
//! - [`inbound`] — where MIDI entering from outside goes. Only the inbound
//!   edge consults it.
//! - [`hardware`] — the machine's MIDI ports: device lifecycle, MIDI-CI, and
//!   every outbound drain toward an OS endpoint.
//! - [`sequence`] and [`file`](mod@self::file) stay ungrouped: beat-scheduled playback is
//!   policy over `endpoint`, and file IO is the `tutti-midi-file` adapter.
//!
//! The dependency arrows only point down the list ([`hardware`] names nothing
//! outside itself; [`inbound`] and [`sequence`] lean on [`endpoint`]), with
//! [`plugin`] as the one file that names every group.
//!
//! # Addressing a synth
//!
//! A MIDI-receiving node owns a [`MidiInPort`](tutti_midi_runtime::MidiInPort):
//! its routing address, its push mailbox, and the slot a beat-scheduled source
//! installs into. The port is captured from the unit as the node is inserted
//! and kept on the entity as a [`MidiTarget`]; every insertion path does this,
//! including a `crossfade`, which replaces a node's unit (and its port) while
//! keeping its `NodeId`. See [`endpoint::target`] for the registry a host fills
//! in — before spawning the node types it names — and
//! [`endpoint::registration`] for how a node's sender gets on (and off) the
//! bus.
//!
//! ```rust
//! use bevy_app::prelude::*;
//! use bevy_tutti::midi::{MidiTargetRegistry, TuttiMidiPlugin};
//!
//! let mut app = App::new();
//! app.insert_resource(bevy_tutti::midi::test_support::midi_bus_for_test());
//! app.add_plugins(bevy_asset::AssetPlugin::default());
//! app.add_plugins(TuttiMidiPlugin);
//!
//! let mut registry = app.world_mut().resource_mut::<MidiTargetRegistry>();
//! // One line per node type the build actually has. See [`endpoint::target`]
//! // for why this cannot be a default list.
//! # #[cfg(feature = "synth")]
//! registry.register::<tutti_polysynth::PolySynth>();
//! # let _ = &mut registry;
//! ```

pub mod endpoint;
pub mod hardware;
pub mod inbound;
pub mod sequence;

pub mod file;

pub mod plugin;

/// Constructors an integration test needs and production code must not have.
///
/// [`MidiBusRes`] has no public constructor on purpose — it must be the instance
/// the RT pre-block shares, and letting a host build one is the silent-failure
/// mode the type exists to prevent. But a test that wants to prove registration
/// works has no audio device and no pre-block, so it needs *some* way in.
///
/// A `#[cfg(test)]` gate would not reach an integration test, which is a
/// separate crate — hence an ordinary public module, documented as test-only and
/// named to make a production call site look wrong in review.
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
        std::sync::Arc<tutti_core::RtPublish<tutti_midi_types::MidiRoutingSnapshot>>,
    ) {
        let table = tutti_midi_types::MidiRoutingTable::new();
        let rt_view = table.snapshot_arc();
        (super::MidiRoutingRes::new(table), rt_view)
    }

    /// Everything sitting in the outbound MIDI-out mailbox, drained.
    ///
    /// **Tests only.** The production drain is
    /// [`pump_midi_out_system`](super::hardware::track_out::pump_midi_out_system),
    /// which routes each event to hardware and keeps none — so a test asserting
    /// *what a producer queued* has to read the mailbox itself, and an
    /// integration test cannot reach `MidiOutRes`'s crate-private receiver.
    pub fn drain_midi_out(
        out: &super::hardware::track_out::MidiOutRes,
    ) -> Vec<tutti_midi_types::ump::MidiEvent> {
        out.drain_for_test()
    }

    /// A clock master wired to a fresh transport. **Tests only.**
    ///
    /// Systems gated on `engine_ready` take this as a plain `Res`, because the
    /// engine block always inserts it — so a test that claims the engine is
    /// running has to supply it or those systems panic on a missing resource.
    pub fn clock_master_for_test(
        sample_rate: impl Into<tutti_core::SampleRate>,
    ) -> super::ClockMasterRes {
        let sample_rate = sample_rate.into();
        let (sender, receiver) =
            tutti_midi_runtime::MidiMailbox::pair(tutti_midi_types::MidiUnitId::next());
        let master = std::sync::Arc::new(tutti_midi_runtime::ClockMaster::new(
            std::sync::Arc::new(tutti_core::transport::Transport::new(sample_rate)),
            sample_rate,
            sender,
        ));
        super::ClockMasterRes::new(master, receiver)
    }
}

pub use endpoint::bus::{MidiBusRes, MpeModeConfig, MpeModeRes};
pub use endpoint::out_sink::MidiOutSinkRes;
pub use endpoint::registration::{
    register_midi_senders, unregister_midi_sender, unregister_removed_midi_target, MidiRegistered,
    MidiRegistrationPlugin,
};
pub use endpoint::target::{MidiNode, MidiTarget, MidiTargetRegistry, MidiTargetResolver};

pub use inbound::route::{
    rebuild as rebuild_midi_routes, MidiRouteFallback, MidiRoutePlugin, MidiRouteRule,
};
pub use inbound::routing_table::MidiRoutingRes;

pub use hardware::clock_out::{pump_clock_out_system, ClockMasterRes, ClockOutPlugin};
#[cfg(feature = "midi-hardware")]
pub use hardware::device::{
    midi_device_connect_system, midi_device_poll_system, ConnectMidiDevice, ConnectMidiOutput,
    DisconnectMidiDevice, DisconnectMidiOutput, MidiDeviceEvent, MidiDevicePlugin, MidiDeviceState,
    MidiDirection, MidiIoRes,
};
#[cfg(all(target_os = "macos", feature = "midi-hardware"))]
pub use hardware::hardware_out::UmpOutRes;
pub use hardware::hardware_out::{
    drain_receiver_through, JrStamperRes, MidiOutDrops, MidiOutRouter,
};
pub use hardware::metadata::{
    flex_metadata_broadcast_system, BroadcastFlexMetadata, MidiMetadataPlugin,
};
pub use hardware::negotiation::{
    ci_discovery_system, ci_ingest_system, endpoint_discovery_system, endpoint_ingest_system,
    CiDeviceDiscovered, CiRes, EndpointDiscovered, EndpointDiscoveryRes, InboundCiMessage,
    InboundEndpointReply, MidiNegotiationPlugin, StartCiDiscovery, StartEndpointDiscovery,
};
pub use hardware::track_out::{
    midi_out_send_system, pump_midi_out_system, MidiOutPlugin, MidiOutRes, SendMidiOut,
};

pub use file::{
    MidiFileAsset, MidiFileAssetLoader, MidiFileContents, MidiFileLoaderError, MidiFilePlugin,
    MidiFileWrite, MidiFileWriteInFlight, MidiFileWritten,
};
pub use plugin::TuttiMidiPlugin;
pub use sequence::{
    rebuild as rebuild_midi_sources, InstalledMidiSources, MidiSequencePlugin, MidiSourceInstall,
};
