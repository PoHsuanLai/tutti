//! Binding the DSP graph to an ECS world.
//!
//! The engine itself needs none of this: the graph is fundsp's
//! [`Net`](tutti_core::dsp::Net), and transport, metering and PDC are plain
//! value types a host can drive directly. This module is the adapter that lets
//! a Bevy `App` reconcile ECS state into that graph:
//!
//! - the graph resources ([`AudioGraphRes`], [`AudioConfig`]),
//! - the reconcile pipeline ([`GraphReconcileSystems`], [`commit_graph`],
//!   [`reconcile_node_despawn`], [`SpawnAudioNode`], [`crossfade_audio_node`])
//!   and its [`GraphReconcilePlugin`],
//! - and the wrappers for metering ([`MeteringRes`]) and transport
//!   ([`TransportRes`], [`MetronomeRes`]).
//!
//! The node handle itself, [`AudioNode`](tutti_core::AudioNode), lives in
//! tutti-core: an entity carrying one *is* a node in the graph.

pub mod metering;
pub mod param;
pub mod plugin;
pub mod reconcile;
pub mod resources;
pub mod transport;

pub use metering::MeteringRes;
pub use param::{reconcile_audio_param, AudioParam, AudioParamAppExt};
pub use plugin::GraphReconcilePlugin;
pub use reconcile::{
    commit_graph, crossfade_audio_node, engine_ready, reconcile_node_despawn, GraphDirty,
    GraphReconcileSystems, SpawnAudioNode,
};
pub use resources::{AudioConfig, AudioGraphRes};
pub use transport::{MetronomeRes, TransportRes};
