//! Binding the DSP graph to an ECS world.
//!
//! The engine itself needs none of this: the graph is fundsp's
//! [`Net`](tutti_core::dsp::Net), and transport, metering and PDC are plain
//! value types a host can drive directly. This module is the adapter that lets
//! a Bevy `App` reconcile ECS state into that graph:
//!
//! - the graph resources ([`AudioGraphRes`], [`AudioConfig`]) in [`resources`],
//! - the pipeline, one file per duty: [`schedule`] (the set order, the
//!   [`engine_ready`] gate, [`GraphDirty`]), [`spawn`] ([`SpawnAudioNode`],
//!   [`crossfade_audio_node`]), [`despawn`] ([`reconcile_node_despawn`]) and
//!   [`commit`] ([`commit_graph`]) — composed by [`GraphReconcilePlugin`],
//! - params ([`AudioParam`]) in [`param`],
//! - the I/O edge ([`AudioPump`]) in [`io`] — an `AudioIn → AudioOut` pump
//!   whose thread the ECS owns, so its sink is finalized exactly once,
//! - and the wrappers for metering ([`MeteringRes`]) and transport
//!   ([`TransportRes`], [`MetronomeRes`]).
//!
//! The node handle itself, [`AudioNode`](tutti_core::AudioNode), lives in
//! tutti-core: an entity carrying one *is* a node in the graph.

pub mod commit;
pub mod despawn;
pub mod io;
pub mod metering;
pub mod param;
pub mod plugin;
pub mod resources;
pub mod schedule;
pub mod spawn;
pub mod tap;
pub mod transport;
pub mod wire;

pub use commit::commit_graph;
pub use despawn::reconcile_node_despawn;
pub use io::{
    drain_audio_pumps, finalize_removed_pumps, AudioPump, AudioPumpAppExt, PumpFinished, IDLE_PARK,
};
pub use metering::MeteringRes;
pub use param::{reconcile_audio_param, AudioParam, AudioParamAppExt};
pub use plugin::GraphReconcilePlugin;
pub use resources::{AudioConfig, AudioGraphRes};
pub use schedule::{engine_ready, GraphDirty, GraphReconcileSystems};
pub use spawn::{crossfade_audio_node, SpawnAudioNode};
pub use tap::AudioTapRes;
pub use transport::{MetronomeRes, TransportRes};
pub use wire::{AudioSource, AudioSources, GraphWirePlugin, MasterSources};
