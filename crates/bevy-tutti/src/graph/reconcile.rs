//! Leaf-specific parameter reconcilers.
//!
//! The generic reconcile hub (`SpawnAudioNode`, `GraphReconcileSystems`,
//! `engine_ready`, `GraphDirty`, `crossfade_audio_node`,
//! `reconcile_node_despawn`, `reconcile_params`, `commit_graph`) moved into
//! [`tutti_core::ecs`]; the DSP / reverb / convolver reconcilers moved into
//! [`tutti_units::ecs`]. This module keeps only the plugin-host reconciler
//! (it writes through `PluginEmitter`, a bevy-tutti-owned type).

// Generic hub items now live in tutti-core; re-export them here so the leaf
// reconcilers below — and the many external `crate::graph::reconcile::*` paths
// — keep resolving against this module.
pub use crate::core::ecs::{
    commit_graph, crossfade_audio_node, engine_ready, reconcile_node_despawn, reconcile_params,
    GraphDirty, GraphReconcileSystems, SpawnAudioNode,
};

#[cfg(feature = "plugin")]
use bevy_ecs::prelude::*;

#[cfg(feature = "plugin")]
use crate::core::ecs::PluginParam;
#[cfg(feature = "plugin")]
use crate::plugin_host::PluginEmitter;

/// Reconciles `Changed<PluginParam>` into the bound [`PluginEmitter`].
///
/// `PluginHandle::set_parameter` is RT-safe fire-and-forget; the call
/// publishes to a lock-free channel that the audio thread drains. No
/// graph mutation happens here, so we don't touch `GraphDirty`.
#[cfg(feature = "plugin")]
pub fn reconcile_plugin_params(
    changed: Query<(&PluginEmitter, &PluginParam), Changed<PluginParam>>,
) {
    for (emitter, param) in changed.iter() {
        emitter.handle.set_parameter(param.id, param.value);
    }
}
