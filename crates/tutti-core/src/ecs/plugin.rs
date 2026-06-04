//! The generic graph-reconcile Bevy plugin and core type registration.
//!
//! [`TuttiGraphPlugin`] wires the four-phase reconcile cycle (`Spawn` →
//! `Params` → `Despawn` → `Commit`) plus the core param-epoch bump, the
//! despawn observer, and the leaf-agnostic reconcile systems. Leaf crates layer
//! their own feature-gated systems on top (sampler/plugin/convolution/midi).

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;

use crate::ecs::param_epoch::{bump_param_epoch_core, NodeParamEpoch};
use crate::ecs::reconcile::{
    commit_graph, engine_ready, reconcile_node_despawn, reconcile_params, GraphDirty,
    GraphReconcileSystems,
};
use crate::ecs::routing::reconcile_audio_routing;
use crate::ecs::sidechain::{reconcile_sidechain_links, reconcile_sidechain_remove};

/// Bevy plugin: the generic graph reconciliation pipeline.
///
/// Runs the four-phase reconcile cycle every `Update`: `Spawn` → `Params`
/// → `Despawn` → `Commit`. Other plugins hook into these sets to interleave
/// their work. This is the leaf-agnostic core; bevy-tutti adds the
/// sampler/plugin/convolution/midi systems on top.
pub struct TuttiGraphPlugin;

impl Plugin for TuttiGraphPlugin {
    fn build(&self, app: &mut App) {
        // Register the core entity-as-node reflectable types (NodeKind, scalar
        // params, construction data). Leaf authoring markers register themselves
        // in their own subsystem plugins. Idempotent.
        register_core_node_types(app);

        app.init_resource::<GraphDirty>()
            .init_resource::<NodeParamEpoch>()
            .configure_sets(
                Update,
                (
                    GraphReconcileSystems::Spawn,
                    GraphReconcileSystems::Params,
                    GraphReconcileSystems::Despawn,
                    GraphReconcileSystems::Commit,
                )
                    .chain(),
            );

        // Per-node param-epoch bumps. Driven by `Changed<T>` on the param
        // components themselves (not the reconcilers), so they fire even when a
        // reconciler doesn't, and can't drift out of sync with the reconciler
        // list. Leaf-family bumps (sampler/plugin/dsp) are added in bevy-tutti.
        app.add_systems(Update, bump_param_epoch_core);

        // Graph-node removal is handled by an `On<Remove, AudioNode>`
        // observer (fires at command-flush, reads the still-present NodeId).
        // Sidechain teardown is handled by an `On<Remove, SidechainOf>`
        // observer; only the *add* half stays a Spawn-set system.
        app.add_observer(reconcile_node_despawn)
            .add_observer(reconcile_sidechain_remove);

        app.add_systems(
            Update,
            (
                reconcile_params.in_set(GraphReconcileSystems::Params),
                commit_graph.in_set(GraphReconcileSystems::Commit),
                reconcile_sidechain_links.in_set(GraphReconcileSystems::Spawn),
                reconcile_audio_routing.in_set(GraphReconcileSystems::Spawn),
            )
                .run_if(engine_ready),
        );
    }
}

/// Register the core entity-as-node reflectable types: `NodeKind`, every
/// scalar param component, and the construction-only authored data.
///
/// Leaf authoring markers (`CompressorNode`, `ReverbNode`, `SamplerNode`, …)
/// are deliberately NOT registered here — bevy-tutti registers those alongside
/// its leaf reconcilers. Idempotent — Bevy's `register_type` ignores
/// duplicates, so calling it from more than one plugin `build()` is harmless.
pub fn register_core_node_types(app: &mut App) {
    use crate::ecs::*;

    app.register_type::<NodeKind>()
        // Scalar params (all carry `Default`).
        .register_type::<Volume>()
        .register_type::<Pan>()
        .register_type::<Mute>()
        .register_type::<Frequency>()
        .register_type::<FilterQ>()
        .register_type::<GainDb>()
        .register_type::<WetMix>()
        .register_type::<Feedback>()
        .register_type::<DelayTime>()
        .register_type::<ModRate>()
        .register_type::<ModDepth>()
        .register_type::<ThresholdDb>()
        .register_type::<CompressorRatio>()
        .register_type::<Attack>()
        .register_type::<Release>()
        .register_type::<CeilingDb>()
        .register_type::<Drive>()
        .register_type::<ReverbRoomSize>()
        .register_type::<ReverbDamping>()
        .register_type::<ReverbAlgo>()
        .register_type::<Azimuth>()
        .register_type::<Elevation>()
        .register_type::<SamplerSpeed>()
        .register_type::<SamplerLooping>()
        .register_type::<ModParam>()
        // Construction-only authored data.
        .register_type::<StereoChannels>()
        .register_type::<MaxDelay>()
        .register_type::<FilterMode>()
        .register_type::<LfoShapeKind>()
        .register_type::<BeatSynced>()
        .register_type::<ReverbTime>();
}
