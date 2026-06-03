//! Leaf-specific parameter reconcilers.
//!
//! The generic reconcile hub (`SpawnAudioNode`, `GraphReconcileSystems`,
//! `engine_ready`, `GraphDirty`, `crossfade_audio_node`,
//! `reconcile_node_despawn`, `reconcile_params`, `commit_graph`) moved into
//! [`tutti_core::ecs`]; the DSP / reverb / convolver reconcilers moved into
//! [`tutti_units::ecs`]; the plugin-host param reconciler
//! (`reconcile_plugin_params`) moved into the `tutti-plugin-host` crate.

// Generic hub items now live in tutti-core; re-export them here so the many
// external `crate::graph::reconcile::*` paths keep resolving against this module.
pub use tutti_core::ecs::{
    commit_graph, crossfade_audio_node, engine_ready, reconcile_node_despawn, reconcile_params,
    GraphDirty, GraphReconcileSystems, SpawnAudioNode,
};
