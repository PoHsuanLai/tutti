//! Re-plan latency compensation when a hosted plugin changes its own latency.
//!
//! # The gap this closes
//!
//! A plugin may change its latency while loaded — a linear-phase EQ switched
//! into an oversampling mode, a look-ahead limiter given a longer window. Every
//! format signals it (VST3 `restartComponent(kLatencyChanged)`, CLAP
//! `clap_host_latency.changed`, AU a `kAudioUnitProperty_Latency` property
//! change), and the engine carries all three to `PluginClient`'s latency atomic.
//!
//! It stops there. Updating what `AudioUnit::latency()` reports does not re-run
//! PDC on its own, so without this system every compensation delay in the graph
//! keeps the figure it was planned against. The result is a plugin whose own
//! latency is right and whose *alignment against every other path* is wrong —
//! audible as a track drifting out of time with the rest of the mix, and
//! self-correcting the next time an unrelated graph edit happens to occur.
//!
//! # Why polling rather than the invalidation callback
//!
//! `PluginHandle::on_invalidate` exists and fires `PluginInvalidation::Latency`
//! from the bridge thread. It is not used here, for two reasons.
//!
//! The callback is documented as emitted **only by the out-of-process backend**,
//! so a subscriber would fix latency re-planning for subprocess-hosted plugins
//! and silently not for in-process ones. The latency atomic is written on both
//! paths.
//!
//! And a callback cannot touch the `World` — it would need a channel and a
//! drain system, which is a second route for a value the graph already holds.
//! Polling reads the one owner. This is the same shape as
//! [`plugin_health_poll`](super::health::plugin_health_poll), which polls the
//! crash flag rather than subscribing to a death notification.
//!
//! # What `CompensatedLatency` owns
//!
//! Not a copy of the plugin's latency — that would be a second owner of a value
//! `PluginClient` already holds, needing invalidation this could not see. It
//! records *what the last compensation pass was planned against*, which is
//! distinct state: the plugin answers "what is my latency now", this answers
//! "what did the graph last align for". A difference between them is exactly
//! the condition that makes the graph stale, and neither value alone expresses
//! it.
//!
//! # What the tests here do not cover
//!
//! `PluginClient::new` launches a plugin-server subprocess, so no unit test can
//! put a real plugin in a graph. The tests below cover the decision rule
//! (`needs_recompensation`) and the two guards that must *not* fire — a
//! non-plugin node and a missing graph. Nothing here observes the flag actually
//! being raised for a live plugin whose latency changed; that needs an
//! integration test loading a real binary, and no such harness exists in this
//! crate yet. Stated rather than implied, because the negative tests pass
//! whether or not the system does anything at all.

use bevy_ecs::prelude::*;

use tutti_core::{AudioNode, Samples};
use tutti_plugin::handles::PluginClient;

use crate::graph::{AudioGraphRes, GraphDirty};

/// The latency the last compensation pass was planned against.
///
/// Inserted by [`plugin_latency_poll`] on first observation, so a plugin that
/// never changes its latency never grows the component. Absence means "not yet
/// compensated for", which is why the first poll marks the graph dirty: the
/// load-time figure was published before any compensation ran.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompensatedLatency(
    /// In [`Samples`], as the plugin's node reports it — *not* the plugin's
    /// current latency, which lives on `PluginClient`.
    pub Samples,
);

/// Marks the graph dirty when a plugin's reported latency no longer matches
/// what compensation was planned against.
///
/// Setting `GraphDirty` is the whole job. `compensate_graph` is gated on that
/// flag rather than on a graph *edit*, so re-planning needs no rewiring — and
/// `commit_graph` clears the flag after publishing, so this must run before the
/// `Compensate` phase to be seen in the same frame.
///
/// Runs over the graph rather than over `PluginEmitter`, because `latency()`
/// lives on the node (`PluginClient`) and not on the handle. An entity whose
/// `AudioNode` is not a `PluginClient` is skipped, which is the same guard
/// `plugin_host::bind` uses: a node can lose its plugin identity between frames.
pub fn plugin_latency_poll(
    mut commands: Commands,
    graph: Option<ResMut<AudioGraphRes>>,
    dirty: Option<ResMut<GraphDirty>>,
    plugins: Query<(Entity, &AudioNode, Option<&CompensatedLatency>)>,
) {
    let (Some(mut graph), Some(mut dirty)) = (graph, dirty) else {
        return;
    };

    for (entity, node, compensated) in plugins.iter() {
        let Some(client) = graph.0.node_as_mut::<PluginClient>(node.0) else {
            continue;
        };
        let current = client.latency();

        if !needs_recompensation(compensated.map(|c| c.0), current) {
            continue;
        }

        commands.entity(entity).insert(CompensatedLatency(current));
        dirty.0 = true;
    }
}

/// Whether a plugin reporting `current` needs the graph re-compensated, given
/// what was last planned against.
///
/// Split out because it is the only part of [`plugin_latency_poll`] reachable
/// without a live plugin: `PluginClient::new` launches a subprocess, so a unit
/// test cannot put one in a graph. Keeping the decision here means the rule can
/// be pinned even though the system around it can only be covered by an
/// integration test that loads a real plugin.
///
/// `None` counts as needing a pass. Nothing has been compensated for this
/// plugin yet, and a plugin that *loads* reporting a non-zero latency needs one
/// exactly as much as a plugin that changes later — treating absence as "no
/// change" would leave every load-time latency uncompensated until the plugin
/// happened to change it.
fn needs_recompensation(compensated: Option<Samples>, current: Samples) -> bool {
    compensated != Some(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AudioEngineState;
    use bevy_app::prelude::*;
    use tutti_core::dsp::Net;

    /// The poll writes `CompensatedLatency` and raises `GraphDirty`; nothing
    /// else in this app does, so both observations are attributable.
    fn test_app() -> App {
        let mut app = App::new();
        app.insert_resource(AudioGraphRes(Net::new(0, 2)));
        app.insert_resource(AudioEngineState::Running);
        app.init_resource::<GraphDirty>();
        app.add_systems(Update, plugin_latency_poll);
        app
    }

    /// An entity whose `AudioNode` is not a `PluginClient` must not raise the
    /// flag — every node in the graph carries `AudioNode`, so a poll that did
    /// not check the type would mark the graph dirty every frame forever and
    /// re-run compensation on a graph nothing had changed.
    #[test]
    fn a_non_plugin_node_never_marks_the_graph_dirty() {
        let mut app = test_app();
        let id = {
            let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
            graph
                .0
                .push(Box::new(tutti_nodes::testing::Const::mono(0.0)))
        };
        let entity = app.world_mut().spawn(AudioNode(id)).id();

        app.update();

        assert!(
            !app.world().resource::<GraphDirty>().0,
            "a dc node is not a PluginClient and has no latency to compensate"
        );
        assert!(
            app.world().get::<CompensatedLatency>(entity).is_none(),
            "nothing should record a compensated latency for a non-plugin node"
        );
    }

    /// The decision table for `needs_recompensation(compensated, current)`.
    ///
    /// One table rather than three functions: every row is the same two-argument
    /// call against a bool, and the interesting content was always the *reasons*,
    /// which the rows now carry directly.
    ///
    /// - The two `false` rows are what keep the system from marking the graph
    ///   dirty every frame: with no comparison, compensation would re-run
    ///   forever and `commit_graph` would republish to the audio thread on every
    ///   tick.
    /// - Both change directions are pinned, because a plugin leaving an
    ///   oversampling mode *shortens* its latency, and a compensation planned
    ///   against the longer figure is as misaligned as one against a shorter.
    /// - The `None` rows are the first pass. The load-time figure is published
    ///   before any compensation runs, so absence cannot be read as "already
    ///   aligned" — and `None` vs `Some(Samples(0))` is exactly the conflation
    ///   that would skip the first pass for plugins reporting latency late.
    #[test]
    fn needs_recompensation_fires_on_any_difference_and_on_the_first_pass() {
        let cases = [
            (
                Some(Samples(512)),
                Samples(512),
                false,
                "unchanged: re-running would dirty the graph every frame",
            ),
            (Some(Samples(0)), Samples(0), false, "unchanged at zero"),
            (Some(Samples(512)), Samples(1024), true, "latency grew"),
            (Some(Samples(1024)), Samples(512), true, "latency shrank"),
            (None, Samples(512), true, "never compensated for"),
            (
                None,
                Samples(0),
                true,
                "absence is not the same as a recorded zero",
            ),
        ];

        for (compensated, current, expected, why) in cases {
            assert_eq!(
                needs_recompensation(compensated, current),
                expected,
                "needs_recompensation({compensated:?}, {current:?}): {why}"
            );
        }
    }

    /// With no graph resource the system is inert rather than panicking.
    ///
    /// `AudioGraphRes` belongs to `engine::build_into` and `GraphDirty` to
    /// `GraphReconcilePlugin`; neither is this module's, so a host that adds
    /// the plugin host alone must not crash.
    #[test]
    fn the_poll_is_inert_without_a_graph() {
        let mut app = App::new();
        app.init_resource::<GraphDirty>();
        app.add_systems(Update, plugin_latency_poll);
        app.update();

        assert!(
            !app.world().resource::<GraphDirty>().0,
            "no graph means nothing to compensate, not a dirty graph"
        );
    }
}
