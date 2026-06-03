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
//! - [`scheduled`] — time-delayed MIDI dispatch (midi-gated).

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;

pub mod param_epoch;
pub mod reconcile;

#[cfg(feature = "sampler")]
pub mod pending_load;
#[cfg(feature = "convolution")]
pub mod pending_convolver;
#[cfg(feature = "midi")]
pub mod scheduled;

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
#[cfg(feature = "sampler")]
pub use reconcile::reconcile_sampler_params;
#[cfg(feature = "plugin")]
pub use reconcile::reconcile_plugin_params;
#[cfg(feature = "dsp")]
pub use reconcile::{reconcile_reverb_params, reconcile_unit_params};

#[cfg(feature = "sampler")]
pub use pending_load::{
    poll_wave_imports, promote_pending_samplers, PendingSamplerLoad, WaveImportQueue,
};
#[cfg(feature = "convolution")]
pub use pending_convolver::{
    promote_pending_convolvers, start_convolver_loads, PendingConvolverLoad,
};
#[cfg(feature = "convolution")]
pub use reconcile::reconcile_convolver_params;
#[cfg(feature = "midi")]
pub use scheduled::{tick_scheduled_midi, MidiSynthMarker, ScheduledMidi};

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
    use crate::core::ecs::*;

    tutti_core::ecs::register_core_node_types(app);

    // Authoring markers. Only the markers a spawn system (`Added<T>`) or a
    // reconciler (`With<T>`) actually reads are kept: the 6 generic
    // `spawn_dsp_node` triggers, the LFO trigger, and the three type-guard
    // markers (Reverb / ConvolutionReverb / Sampler).
    app.register_type::<CompressorNode>()
        .register_type::<GateNode>()
        .register_type::<FilterNode>()
        .register_type::<ReverbNode>()
        .register_type::<ConvolutionReverbNode>()
        .register_type::<DelayNode>()
        .register_type::<ChorusNode>()
        .register_type::<SamplerNode>()
        .register_type::<LfoNodeMarker>();
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
        #[cfg(feature = "sampler")]
        app.add_systems(Update, param_epoch::bump_param_epoch_sampler);
        #[cfg(feature = "plugin")]
        app.add_systems(Update, param_epoch::bump_param_epoch_plugin);

        #[cfg(feature = "sampler")]
        {
            app.init_resource::<WaveImportQueue>().add_systems(
                Update,
                (
                    reconcile::reconcile_sampler_volume.in_set(GraphReconcileSystems::Params),
                    reconcile_sampler_params.in_set(GraphReconcileSystems::Params),
                    promote_pending_samplers
                        .after(poll_wave_imports)
                        .in_set(GraphReconcileSystems::Spawn),
                )
                    .run_if(engine_ready),
            );
            // `poll_wave_imports` only touches `WaveImportQueue` + `Assets`, not
            // an engine resource, so it stays ungated.
            app.add_systems(Update, poll_wave_imports);
        }

        #[cfg(feature = "convolution")]
        {
            // `start_convolver_loads` only uses `AssetServer` (not an engine
            // resource); `promote_pending_convolvers` needs the graph.
            app.add_systems(Update, start_convolver_loads);
            app.add_systems(
                Update,
                promote_pending_convolvers
                    .after(start_convolver_loads)
                    .in_set(GraphReconcileSystems::Spawn)
                    .run_if(engine_ready),
            );
        }

        // `reconcile_plugin_params` writes through `PluginEmitter`, holds no
        // engine resource, so it stays ungated.
        #[cfg(feature = "plugin")]
        app.add_systems(
            Update,
            reconcile_plugin_params.in_set(GraphReconcileSystems::Params),
        );

        #[cfg(feature = "midi")]
        app.add_systems(Update, tick_scheduled_midi.run_if(engine_ready));
    }
}
