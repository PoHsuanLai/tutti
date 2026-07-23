//! The Bevy ECS integration layer — all of tutti-core's `#[cfg(feature = "bevy")]`
//! surface gathered in one place.
//!
//! **The engine does not need this.** The DSP graph is fundsp's
//! [`Net`](crate::dsp::Net); transport, metering and PDC are plain value types.
//! A non-Bevy host drives them through their own APIs directly. This module is
//! only the *adapter* that lets a Bevy `App` reconcile ECS state into the graph:
//!
//! - the graph resources ([`AudioGraphRes`], [`AudioConfig`], [`PendingGraph`]),
//! - the four-phase reconcile pipeline ([`GraphReconcileSystems`],
//!   [`commit_graph`], [`reconcile_node_despawn`], [`SpawnAudioNode`],
//!   [`crossfade_audio_node`]) and its [`GraphReconcilePlugin`],
//! - the per-node param epoch ([`NodeParamEpoch`]; the bump systems that read
//!   the DAW param components live app-side now),
//! - the audio-emitter markers ([`AudioEmitter`], [`AudioPlaybackState`]),
//! - and the per-subsystem Bevy wrappers for metering ([`MeteringRes`]) and
//!   transport ([`TransportRes`], [`MetronomeRes`]).
//!
//! The param *component types* themselves ([`AudioNode`](crate::graph::AudioNode),
//! [`Volume`](crate::graph::Volume), …) stay in [`crate::graph`] — they are
//! always-compiled plain data whose Bevy `derive`s are feature-gated in place,
//! so they degrade to plain structs without this module.
//!
//! Everything here is re-exported from its historical path
//! (`tutti_core::graph::*`, `tutti_core::metering::*`, `tutti_core::transport::*`)
//! so existing imports keep resolving; this module is the physical home, those
//! are the compatibility surface.

pub mod emitter;
pub mod metering;
pub mod param_epoch;
pub mod plugin;
pub mod reconcile;
pub mod resources;
pub mod transport;

pub use emitter::{AudioEmitter, AudioPlaybackState};
pub use metering::{MeteringRes, PendingMetering, TuttiMeteringPlugin};
pub use param_epoch::NodeParamEpoch;
pub use plugin::GraphReconcilePlugin;
pub use reconcile::{
    commit_graph, crossfade_audio_node, engine_ready, reconcile_node_despawn, GraphDirty,
    GraphReconcileSystems, SpawnAudioNode,
};
pub use resources::{AudioConfig, AudioGraphRes, PendingGraph};
pub use transport::{
    MetronomeRes, PendingMetronome, PendingTransport, TransportClockNode, TransportRes,
    TuttiTransportPlugin,
};
