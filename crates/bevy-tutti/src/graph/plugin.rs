//! The graph-reconcile Bevy plugin.
//!
//! [`GraphReconcilePlugin`] wires the five-phase reconcile cycle (`Spawn` →
//! `Params` → `Despawn` → `Compensate` → `Commit`), the despawn observer, and
//! the reconcile systems every host needs. The feature-gated subsystem plugins
//! (sampler, MIDI, plugin hosting, modulation) schedule their own systems into
//! the same sets.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;

use crate::graph::{
    commit_graph, engine_ready, reconcile_node_despawn, GraphDirty, GraphReconcileSystems,
};

/// Reconciles declared ECS state into the audio graph every frame.
///
/// Runs the five-phase reconcile cycle in `Update`: `Spawn` → `Params` →
/// `Despawn` → `Compensate` → `Commit` ([`GraphReconcileSystems`]), gated on
/// [`engine_ready`]. It adds the declared-wiring plugins
/// ([`GraphWirePlugin`](crate::graph::GraphWirePlugin),
/// [`GraphEventsPlugin`](crate::graph::GraphEventsPlugin)), the
/// [`reconcile_node_despawn`] observer, [`commit_graph`], and initializes
/// [`GraphDirty`], [`ChannelCompensation`](crate::ChannelCompensation) and
/// [`GraphLatency`](crate::GraphLatency).
///
/// [`TuttiPlugin`](crate::TuttiPlugin) always adds it. Other plugins hook into
/// the sets to interleave their work; `Compensate` holds only
/// [`LatencyCompensationPlugin`](crate::LatencyCompensationPlugin)'s debug
/// check, if a host adds it (the figures are published in `Commit`).
pub struct GraphReconcilePlugin;

impl Plugin for GraphReconcilePlugin {
    fn build(&self, app: &mut App) {
        // The DAW param components (`Volume`/`Pan`/`Mute`/`PluginParam`/`ModParam`)
        // are a host's — this crate carries none of them. `AudioParam<U, P>` (see
        // `graph::param`) is the generic one here.

        // The PDC figures `commit_graph` publishes with every plan. `init`, so
        // `build_into`'s `ChannelCompensation` (the one the sampler holds)
        // is kept.
        app.init_resource::<crate::graph::latency::ChannelCompensation>()
            .init_resource::<crate::graph::latency::GraphLatency>();
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
        // observer (fires at command-flush, reads the still-present NodeKey).
        app.add_observer(reconcile_node_despawn);

        // Declared wiring: `PortSources` per sink, `MasterSources` for the bus.
        app.add_plugins(crate::graph::GraphWirePlugin);
        app.add_plugins(crate::graph::GraphEventsPlugin);

        // Crossfades that waited out a re-prepare land first thing in the
        // frame, with the frame's own spawns.
        app.init_resource::<crate::graph::PendingCrossfades>();
        app.add_systems(
            Update,
            crate::graph::spawn::retry_pending_crossfades
                .in_set(GraphReconcileSystems::Spawn)
                .run_if(engine_ready),
        );

        app.add_systems(
            Update,
            commit_graph
                .in_set(GraphReconcileSystems::Commit)
                .run_if(engine_ready),
        );
    }
}
