//! The audio graph: runtime + (optional) Bevy ECS reconcile hub.
//!
//! The editable DSP graph itself is always compiled — [`AudioGraph`]
//! (`editable`) over the [`GraphNet`] fundsp facade (`net`) — and carries no
//! Bevy dependency. A non-Bevy host wires nodes through [`AudioGraph`]'s
//! imperative API (`connect`/`disconnect`/`add`/`remove`) directly.
//!
//! The parameter *types* ([`AudioNode`], [`NodeKind`], [`Volume`], [`Pan`],
//! [`Mute`], [`PluginParam`], [`ModParam`], [`LayerKey`]) live in [`params`]
//! and are always compiled as plain structs; under the `bevy` feature they
//! gain `#[derive(Component, Reflect)]`.
//!
//! Everything else in this module is the Bevy ECS integration — the reconcile
//! pipeline (reconcile / routing / sidechain relationships / emitter markers /
//! resources / `GraphReconcilePlugin`) — and is gated behind the `bevy`
//! feature. It translates ECS component/relationship changes into
//! [`AudioGraph`] operations; nothing in the runtime calls back into ECS.
//! Leaf-specific reconcilers (sampler/plugin/convolution/midi) stay in
//! bevy-tutti.

// Always-compiled runtime + parameter types.
pub mod editable;
pub mod net;
pub mod params;

pub use editable::{isolate_output, AudioGraph, GraphDot};
pub use net::{CommitOutcome, GraphNet};
pub use params::{AudioNode, LayerKey, ModParam, Mute, NodeKind, Pan, PluginParam, Volume};

// Bevy ECS reconcile hub — only compiled with the `bevy` feature.
#[cfg(feature = "bevy")]
pub mod emitter;
#[cfg(feature = "bevy")]
pub mod param_epoch;
#[cfg(feature = "bevy")]
pub mod plugin;
#[cfg(feature = "bevy")]
pub mod reconcile;
#[cfg(feature = "bevy")]
pub mod resources;
#[cfg(feature = "bevy")]
pub mod routing;
#[cfg(feature = "bevy")]
pub mod sidechain;

#[cfg(feature = "bevy")]
pub use emitter::{AudioEmitter, AudioPlaybackState};
#[cfg(feature = "bevy")]
pub use param_epoch::{bump_param_epoch_core, NodeParamEpoch};
#[cfg(feature = "bevy")]
pub use plugin::{register_core_node_types, GraphReconcilePlugin};
#[cfg(feature = "bevy")]
pub use reconcile::{
    commit_graph, crossfade_audio_node, engine_ready, reconcile_node_despawn, reconcile_params,
    GraphDirty, GraphReconcileSystems, SpawnAudioNode,
};
#[cfg(feature = "bevy")]
pub use resources::{AudioConfig, AudioGraphRes, PendingGraph};
// `TransportRes`/`MeteringRes` and their plugins now live next to their own
// subsystem (the `bevy_audio`-style per-subsystem co-location). Re-exported here
// so existing `tutti_core::graph::{TransportRes, MeteringRes}` paths keep resolving.
#[cfg(feature = "bevy")]
pub use crate::metering::{MeteringRes, PendingMetering, TuttiMeteringPlugin};
#[cfg(feature = "bevy")]
pub use crate::transport::{PendingTransport, TransportRes, TuttiTransportPlugin};
#[cfg(feature = "bevy")]
pub use routing::{reconcile_audio_routing, AudioFedBy, AudioFeedsTo};
#[cfg(feature = "bevy")]
pub use sidechain::{
    reconcile_sidechain_links, reconcile_sidechain_remove, SidechainOf, SidechainSources,
};
