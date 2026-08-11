//! The generic graph-reconcile Bevy plugin and core type registration.
//!
//! [`GraphReconcilePlugin`] wires the five-phase reconcile cycle (`Spawn` →
//! `Params` → `Despawn` → `Compensate` → `Commit`), the despawn observer, and
//! the leaf-agnostic reconcile systems. Leaf crates layer their own
//! feature-gated systems on top (sampler/plugin/convolution/midi).

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;

use crate::graph::{
    commit_graph, engine_ready, reconcile_node_despawn, GraphDirty, GraphReconcileSystems,
};

/// Bevy plugin: the generic graph reconciliation pipeline.
///
/// Runs the five-phase reconcile cycle every `Update`: `Spawn` → `Params` →
/// `Despawn` → `Compensate` → `Commit`. Other plugins hook into these sets to
/// interleave their work; `Compensate` stays empty unless a host adds
/// [`LatencyCompensationPlugin`](crate::LatencyCompensationPlugin). This is the
/// leaf-agnostic core; bevy-tutti adds the sampler/plugin/convolution/midi
/// systems on top.
pub struct GraphReconcilePlugin;

impl Plugin for GraphReconcilePlugin {
    fn build(&self, app: &mut App) {
        // The DAW param components (`Volume`/`Pan`/`Mute`/`PluginParam`/`ModParam`)
        // are app-side, in `dawai_model::audio_graph` — this crate carries none of
        // them. `AudioParam<U, P>` (see `graph::param`) is the generic one here.

        app.init_resource::<GraphDirty>().configure_sets(
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

        // `AudioGraphRes` + `AudioConfig` are inserted directly by `build_into`
        // before this plugin is added; there is no transient to claim.

        // Graph-node removal is handled by an `On<Remove, AudioNode>`
        // observer (fires at command-flush, reads the still-present NodeId).
        app.add_observer(reconcile_node_despawn);

        // Declared wiring: `AudioSources` per sink, `MasterSources` for the bus.
        app.add_plugins(crate::graph::GraphWirePlugin);

        app.add_systems(
            Update,
            commit_graph
                .in_set(GraphReconcileSystems::Commit)
                .run_if(engine_ready),
        );
    }
}
