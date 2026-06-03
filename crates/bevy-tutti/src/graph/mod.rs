//! Graph reconciliation: entity-as-node component changes → tutti graph ops.
//!
//! The keystone duty for `bevy-tutti`. The generic reconcile hub
//! (`GraphReconcileSystems`, `SpawnAudioNode`, `engine_ready`, `GraphDirty`,
//! `crossfade_audio_node`, the despawn observer, `reconcile_params`,
//! `commit_graph`, the routing/sidechain relationships, and the core
//! `TuttiGraphPlugin`) moved into [`tutti_core::ecs`]. This module re-exports
//! all of that and adds the leaf-specific reconcilers + their schedules on top.
//!
//! Sub-concepts that remain here:
//! - [`reconcile`] — leaf param reconcilers (sampler / plugin / dsp / reverb /
//!   convolver).
//! - [`param_epoch`] — leaf-family param-epoch bumps (core bump is in tutti-core).
//! - [`pending_load`] — sampler pending-load promotion (sampler-gated).
//!
//! Time-delayed MIDI dispatch (`ScheduledMidi` / `tick_scheduled_midi`) now
//! lives in [`tutti_midi_io::ecs`] and is scheduled by its `TuttiMidiPlugin`.

use bevy_app::{App, Plugin};
#[cfg(feature = "plugin")]
use bevy_app::Update;
#[cfg(feature = "plugin")]
use bevy_ecs::prelude::*;

pub mod param_epoch;
pub mod reconcile;

// Generic hub items re-exported from tutti-core's ECS module so existing
// `crate::graph::*` paths hold unchanged.
pub use tutti_core::ecs::{
    commit_graph, crossfade_audio_node, engine_ready, reconcile_node_despawn, reconcile_params,
    GraphDirty, GraphReconcileSystems, SpawnAudioNode, NodeParamEpoch,
};
pub use tutti_core::ecs::{
    reconcile_audio_routing, reconcile_sidechain_links, reconcile_sidechain_remove, AudioFedBy,
    AudioFeedsTo, SidechainOf, SidechainSources,
};
pub use param_epoch::bump_param_epoch_core;

// Leaf reconcilers (stay defined in bevy-tutti).
#[cfg(feature = "plugin")]
pub use reconcile::reconcile_plugin_params;

// DSP param reconcilers + the convolver reconciler + pending-convolver load now
// live in `tutti_units::ecs`; re-export them here so the legacy
// `crate::graph::*` paths and the prelude keep resolving.
#[cfg(feature = "dsp")]
pub use tutti_units::ecs::{reconcile_reverb_params, reconcile_unit_params};
#[cfg(feature = "convolution")]
pub use tutti_units::ecs::{
    promote_pending_convolvers, reconcile_convolver_params, start_convolver_loads,
    PendingConvolverLoad,
};

/// Register every reflectable entity-as-node component (params, construction
/// data, and authoring markers) for the type registry.
///
/// Delegates the core-owned types (`NodeKind`, scalar params, construction
/// data) to [`tutti_core::ecs::register_core_node_types`], then registers the
/// leaf authoring markers (`CompressorNode`, …) that stay owned here.
/// Idempotent — Bevy's `register_type` ignores duplicates, so calling it from
/// more than one plugin `build()` is harmless.
///
/// `AudioNode` is deliberately not registered (it wraps a foreign non-`Reflect`
/// `NodeId`). The deliberately-non-`Reflect` set (AudioEmitter, PluginEmitter,
/// TrackClipReader*, …) is owned by other duties and skipped.
pub fn register_audio_node_types(app: &mut App) {
    // The DSP authoring markers (`CompressorNode`, …, `LfoNodeMarker`) are now
    // registered by `tutti_units::ecs::TuttiDspPlugin`; the `SamplerNode` marker
    // by `tutti_sampler::ecs::TuttiSamplerPlugin`. Here we only register the
    // core-owned types (`NodeKind`, scalar params, construction data).
    tutti_core::ecs::register_core_node_types(app);
}

/// Bevy plugin: graph reconciliation pipeline.
///
/// Adds the generic core plugin ([`tutti_core::ecs::TuttiGraphPlugin`]) which
/// configures the four-phase cycle, the core param-epoch bump, the despawn +
/// sidechain-remove observers, and the leaf-agnostic reconcile systems. Then
/// layers the feature-gated leaf schedules (sampler / convolution / plugin /
/// midi) on top.
pub struct TuttiGraphPlugin;

impl Plugin for TuttiGraphPlugin {
    fn build(&self, app: &mut App) {
        // Generic core hub: sets, GraphDirty, NodeParamEpoch, bump_param_epoch_core,
        // despawn + sidechain-remove observers, reconcile_params / commit_graph /
        // reconcile_sidechain_links / reconcile_audio_routing.
        app.add_plugins(tutti_core::ecs::TuttiGraphPlugin);

        // Leaf-family param-epoch bumps (core bump added by the core plugin).
        // The sampler bump + sampler reconcilers + pending-load promotion now
        // live in `tutti_sampler::ecs::TuttiSamplerPlugin`.
        #[cfg(feature = "plugin")]
        app.add_systems(Update, param_epoch::bump_param_epoch_plugin);

        // The convolution ECS surface (pending-load promotion + the convolver
        // param reconciler) now lives in `tutti_units::ecs::TuttiDspPlugin`.

        // `reconcile_plugin_params` writes through `PluginEmitter`, holds no
        // engine resource, so it stays ungated.
        #[cfg(feature = "plugin")]
        app.add_systems(
            Update,
            reconcile_plugin_params.in_set(GraphReconcileSystems::Params),
        );
    }
}
