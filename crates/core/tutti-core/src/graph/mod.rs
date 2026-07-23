//! The audio graph: parameter types + (optional) Bevy ECS reconcile hub.
//!
//! **The graph itself is fundsp's [`Net`](crate::dsp::Net)** — there is no
//! tutti wrapper around it. A host wires nodes through `Net`'s imperative API
//! (`add`/`connect`/`disconnect`/`remove`) and calls `commit()` to publish a
//! batch of edits to the audio thread. Everything tutti used to add on top
//! (typed node access, unboxed `add`, output isolation, a readable sample rate)
//! now lives on `Net` itself.
//!
//! The parameter *types* ([`AudioNode`], [`NodeKind`], [`Volume`], [`Pan`],
//! [`Mute`], [`PluginParam`], [`ModParam`], [`LayerKey`]) live in [`params`]
//! and are always compiled as plain structs; under the `bevy` feature they
//! gain `#[derive(Component, Reflect)]`.
//!
//! Everything else in this module is the Bevy ECS integration — the reconcile
//! pipeline (spawn / despawn / commit / emitter markers / resources /
//! `GraphReconcilePlugin`) — and is gated behind the `bevy` feature. It
//! translates ECS component changes into `Net` operations; nothing in the
//! runtime calls back into ECS. Edges are wired by the host (dawai's
//! `Connection` model); leaf-specific reconcilers (sampler/plugin/convolution/
//! midi) stay in bevy-tutti.

// Always-compiled parameter types.
pub mod params;

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
pub use emitter::{AudioEmitter, AudioPlaybackState};
#[cfg(feature = "bevy")]
pub use param_epoch::{bump_param_epoch_core, NodeParamEpoch};
#[cfg(feature = "bevy")]
pub use plugin::{register_core_node_types, GraphReconcilePlugin};
#[cfg(feature = "bevy")]
pub use reconcile::{
    commit_graph, crossfade_audio_node, engine_ready, reconcile_node_despawn, GraphDirty,
    GraphReconcileSystems, SpawnAudioNode,
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
