//! The generic graph-reconcile Bevy plugin and core type registration.
//!
//! [`GraphReconcilePlugin`] wires the four-phase reconcile cycle (`Spawn` →
//! `Params` → `Despawn` → `Commit`) plus the core param-epoch bump, the
//! despawn observer, and the leaf-agnostic reconcile systems. Leaf crates layer
//! their own feature-gated systems on top (sampler/plugin/convolution/midi).

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;

use crate::graph::param_epoch::{bump_param_epoch_core, NodeParamEpoch};
use crate::graph::reconcile::{
    commit_graph, engine_ready, reconcile_node_despawn, reconcile_params, GraphDirty,
    GraphReconcileSystems,
};
use crate::graph::resources::{AudioGraphRes, PendingGraph};
use crate::graph::routing::reconcile_audio_routing;
use crate::graph::sidechain::{reconcile_sidechain_links, reconcile_sidechain_remove};

/// Bevy plugin: the generic graph reconciliation pipeline.
///
/// Runs the four-phase reconcile cycle every `Update`: `Spawn` → `Params`
/// → `Despawn` → `Commit`. Other plugins hook into these sets to interleave
/// their work. This is the leaf-agnostic core; bevy-tutti adds the
/// sampler/plugin/convolution/midi systems on top.
pub struct GraphReconcilePlugin;

impl Plugin for GraphReconcilePlugin {
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
                    GraphReconcileSystems::Compensate,
                    GraphReconcileSystems::Commit,
                )
                    .chain(),
            );

        // Claim the graph + config out of the transient `build_into` inserted
        // (synchronous, during plugin build — see `claim_pending`). The graph
        // subsystem owns its own claim, like every other subsystem plugin.
        if let Some(PendingGraph(Some((graph, config)))) =
            app.world_mut().remove_resource::<PendingGraph>()
        {
            app.insert_resource(AudioGraphRes(graph));
            app.insert_resource(config);
        }

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

/// Register the core entity-as-node reflectable types: `NodeKind` + the
/// foundational graph params (`Volume`/`Pan`/`Mute`/`PluginParam`/`ModParam`).
///
/// The DSP param pool + node markers live in tutti-units (registered by
/// `TuttiDspPlugin`); the sampler params/marker in tutti-sampler. Idempotent —
/// Bevy's `register_type` ignores duplicates.
pub fn register_core_node_types(app: &mut App) {
    use crate::graph::*;

    app.register_type::<NodeKind>()
        .register_type::<Volume>()
        .register_type::<Pan>()
        .register_type::<Mute>()
        .register_type::<PluginParam>()
        .register_type::<ModParam>();
}
