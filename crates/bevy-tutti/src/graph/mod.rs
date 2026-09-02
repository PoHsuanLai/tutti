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
//! - the graph as a **value** ([`LiveGraph`]) in [`topology`] — built in the
//!   same phase that wires, so every static question (latency, tail, "did
//!   anything change") is a fold over a value a test can write down,
//! - the I/O edge ([`AudioPump`]) in [`pump`] — an `AudioIn → AudioOut` pump
//!   whose thread the ECS owns, so its sink is finalized exactly once,
//! - opt-in latency (PDC) compensation in [`latency`] — the read side of
//!   tutti-core's PDC, run in [`GraphReconcileSystems::Compensate`],
//! - and the wrappers for metering ([`MeteringRes`]) and transport
//!   ([`TransportRes`], [`MetronomeRes`]).
//!
//! The node handle itself, [`AudioNode`](tutti_core::AudioNode), lives in
//! tutti-core: an entity carrying one *is* a node in the graph.

pub mod commit;
pub mod despawn;
pub mod latency;
pub mod metering;
pub mod param;
pub mod plugin;
pub mod pump;
pub mod resources;
pub mod schedule;
pub mod spawn;
pub mod tap;
pub mod topology;
pub mod transport;
pub mod wire;

pub use commit::commit_graph;
pub use despawn::reconcile_node_despawn;
pub use metering::MeteringRes;
pub use param::{reconcile_audio_param, write_param, AudioParam, AudioParamAppExt};
pub use plugin::GraphReconcilePlugin;
pub use pump::{
    drain_audio_pumps, finalize_removed_pumps, AudioPump, AudioPumpAppExt, PumpFinished, IDLE_PARK,
};
pub use resources::{AudioConfig, AudioGraphRes};
pub use schedule::{engine_ready, GraphDirty, GraphReconcileSystems};
pub use spawn::{crossfade_audio_node, InsertAudioNode, SpawnAudioNode};
pub use tap::AudioTapRes;
pub use topology::LiveGraph;
pub use transport::{EngineNodes, MetronomeRes, TransportRes};
pub use wire::{GraphWirePlugin, MasterSources, PortSource, PortSources};
