//! The generic graph-reconcile Bevy plugin and core type registration.
//!
//! [`GraphReconcilePlugin`] wires the four-phase reconcile cycle (`Spawn` →
//! `Params` → `Despawn` → `Commit`) plus the core param-epoch bump, the
//! despawn observer, and the leaf-agnostic reconcile systems. Leaf crates layer
//! their own feature-gated systems on top (sampler/plugin/convolution/midi).

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;

use crate::ecs::param_epoch::NodeParamEpoch;
use crate::ecs::reconcile::{
    commit_graph, engine_ready, reconcile_node_despawn, GraphDirty, GraphReconcileSystems,
};
use crate::ecs::resources::{AudioGraphRes, PendingGraph};

/// Bevy plugin: the generic graph reconciliation pipeline.
///
/// Runs the four-phase reconcile cycle every `Update`: `Spawn` → `Params`
/// → `Despawn` → `Commit`. Other plugins hook into these sets to interleave
/// their work. This is the leaf-agnostic core; bevy-tutti adds the
/// sampler/plugin/convolution/midi systems on top.
pub struct GraphReconcilePlugin;

impl Plugin for GraphReconcilePlugin {
    fn build(&self, app: &mut App) {
        // The DAW param components (`Volume`/`Pan`/`Mute`/`PluginParam`/`ModParam`)
        // + their reflection registration + the `Changed<T>` epoch bumps all moved
        // app-side (`dawai_model::engine_bind`) — the pump carries none of them.

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

        // Graph-node removal is handled by an `On<Remove, AudioNode>`
        // observer (fires at command-flush, reads the still-present NodeId).
        app.add_observer(reconcile_node_despawn);

        app.add_systems(
            Update,
            commit_graph
                .in_set(GraphReconcileSystems::Commit)
                .run_if(engine_ready),
        );
    }
}
