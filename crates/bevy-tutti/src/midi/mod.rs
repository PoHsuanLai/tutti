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
//! Within the tier, the modules group by duty, and each one's rule is one
//! sentence:
//!
//! - [`nodes`] — the engine's MIDI graph nodes (hardware input, clock,
//!   hardware out) and a keyboard's way in ([`LiveMidiInput`]).
//! - [`inbound`] — where MIDI entering from the hardware input goes: route
//!   rules, wired to the input node's per-channel ports.
//! - [`hardware`] — the machine's MIDI ports: device lifecycle, MIDI-CI, and
//!   every outbound drain toward an OS endpoint.
//! - [`sequence`] and [`file`](mod@self::file) stay ungrouped: beat-scheduled
//!   playback (a clip node per target) and the `tutti-midi-file` adapter.
//!
//! # MIDI is wiring
//!
//! A MIDI-receiving node (a synth, a SoundFont player, a hosted plugin) takes
//! MIDI on its event input, and nothing else reaches it (doc 013, rewrite
//! item 5). What feeds that input is declared: `EventSources` on the entity,
//! a [`MidiRouteRule`] for the hardware input, a [`MidiSourceInstall`] for a
//! clip, a [`LiveMidiInput`] for a keyboard. The graph merges them by frame.
//!
//! ```rust
//! use bevy_app::prelude::*;
//! use bevy_ecs::prelude::*;
//! use bevy_tutti::midi::{LiveMidiInput, TuttiMidiPlugin};
//!
//! let mut app = App::new();
//! app.add_plugins(bevy_asset::AssetPlugin::default());
//! app.add_plugins(TuttiMidiPlugin);
//! // A keyboard for a synth entity: once the engine runs, the entity gets a
//! // `LiveMidi` sender wired to its event input.
//! app.world_mut().spawn(LiveMidiInput);
//! ```

pub mod hardware;
pub mod inbound;
pub mod nodes;
pub mod sequence;

pub mod file;

pub mod plugin;

/// Constructors an integration test needs and production code must not have.
///
/// A `#[cfg(test)]` gate would not reach an integration test, which is a
/// separate crate — hence an ordinary public module, documented as test-only
/// and named to make a production call site look wrong in review.
pub mod test_support {
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

    /// A clock master and a hardware-out ring, in no graph. **Tests only.**
    ///
    /// Systems gated on `engine_ready` take this as a plain `Res`, because the
    /// engine block always inserts it — so a test that claims the engine is
    /// running has to supply it or those systems panic on a missing resource.
    pub fn clock_master_for_test() -> super::ClockMasterRes {
        use tutti_graph::IntoNode;
        let master = tutti_midi_runtime::ClockNode::new().into_parts().controls;
        let out = tutti_midi_runtime::MidiOutNode::new().into_parts().controls;
        super::ClockMasterRes::new(master, out)
    }
}

pub use nodes::{
    attach_live_midi, detach_live_midi, LiveMidi, LiveMidiInput, LiveMidiPlugin, MidiEngineNodes,
    MpeModeConfig, MpeModeRes,
};

pub use inbound::route::{
    rebuild as rebuild_midi_routes, MidiRouteFallback, MidiRoutePlugin, MidiRouteRule,
    RoutedTargets,
};

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
    rebuild as rebuild_midi_sources, MidiSequencePlugin, MidiSourceInstall, SequencedClips,
};
