//! Graph-plugin assembly for the engine composition root.
//!
//! The generic reconcile hub (`GraphReconcileSystems`, `SpawnAudioNode`,
//! `engine_ready`, `GraphDirty`, the despawn/sidechain observers,
//! `reconcile_params`, `commit_graph`, routing/sidechain, the core
//! `TuttiGraphPlugin`) lives in [`tutti_core::ecs`]; the leaf reconcilers live
//! in their subsystem crates (`tutti_units::ecs`, `tutti_sampler::ecs`,
//! `tutti_plugin_host`, `tutti_midi_io::ecs`) and are scheduled by those
//! crates' own plugins. This module only assembles [`TuttiGraphPlugin`] for the
//! composition root.

use bevy_app::{App, Plugin};

// `plugin.rs` imports `engine_ready` from here alongside `TuttiGraphPlugin`.
pub use tutti_core::ecs::engine_ready;

/// Bevy plugin: graph reconciliation pipeline.
///
/// Adds the generic core plugin ([`tutti_core::ecs::TuttiGraphPlugin`]) which
/// configures the four-phase cycle, the core param-epoch bump, the despawn +
/// sidechain-remove observers, and the leaf-agnostic reconcile systems. The
/// feature-gated leaf schedules (sampler / dsp / convolution / plugin / midi)
/// are added by their own subsystem plugins, composed in [`crate::TuttiPlugin`].
pub struct TuttiGraphPlugin;

impl Plugin for TuttiGraphPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(tutti_core::ecs::TuttiGraphPlugin);
    }
}
