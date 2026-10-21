//! The graph-reconcile Bevy plugin.
//!
//! [`GraphReconcilePlugin`] wires the five-phase reconcile cycle (`Spawn` →
//! `Params` → `Despawn` → `Compensate` → `Commit`), the despawn observer, and
//! the reconcile systems every host needs. The feature-gated subsystem plugins
//! (sampler, MIDI, plugin hosting, modulation) schedule their own systems into
//! the same sets.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;

use tutti_session::Phase;

use crate::graph::schedule::configure_phases;
use crate::graph::{
    collect_graph, commit_graph, engine_ready, reclaim_graph, reconcile_node_despawn,
    GraphReconcileSystems, SessionPhaseSystems,
};

/// Reconciles declared ECS state into the audio graph every frame.
///
/// Runs the five-phase reconcile cycle in `Update`: `Spawn` → `Params` →
/// `Despawn` → `Compensate` → `Commit` ([`GraphReconcileSystems`]), gated on
/// [`engine_ready`]. It adds the declared-wiring plugins
/// ([`GraphWirePlugin`](crate::graph::GraphWirePlugin),
/// [`GraphEventsPlugin`](crate::graph::GraphEventsPlugin)), the
/// [`reconcile_node_despawn`] observer, [`collect_graph`] and
/// [`commit_graph`], [`reclaim_graph`], the session's phases in `PHASES` order
/// ([`SessionPhaseSystems`]), and initializes
/// [`ChannelCompensation`](crate::ChannelCompensation),
/// [`GraphLatency`](crate::GraphLatency) and
/// [`PendingCrossfades`](crate::graph::PendingCrossfades).
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
        app.configure_sets(
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
        // The session's phases, in `PHASES` order, placed against the sets
        // above (`SessionPhaseSystems`).
        configure_phases(app);

        // `AudioGraphRes` + `AudioConfig` are inserted directly by `build_into`
        // before this plugin is added; there is no transient to claim.

        // Graph-node removal is handled by an `On<Remove, AudioNode>`
        // observer (fires at command-flush, reads the still-present NodeKey).
        app.add_observer(reconcile_node_despawn);

        // Declared wiring: `PortSources` per sink, `MasterSources` for the bus.
        app.add_plugins(crate::graph::GraphWirePlugin);
        app.add_plugins(crate::graph::GraphEventsPlugin);

        // Crossfades waiting out a re-prepare: the session retries them in
        // its `Collect` phase (`collect_graph`), which binds each one's
        // controls to its entity once it lands.
        app.init_resource::<crate::graph::PendingCrossfades>();

        app.add_systems(
            Update,
            (
                collect_graph.in_set(SessionPhaseSystems(Phase::Collect)),
                commit_graph.in_set(SessionPhaseSystems(Phase::Commit)),
                reclaim_graph.in_set(SessionPhaseSystems(Phase::Reclaim)),
            )
                .run_if(engine_ready),
        );
    }
}
