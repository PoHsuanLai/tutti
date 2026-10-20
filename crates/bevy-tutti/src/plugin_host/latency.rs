//! Re-planning latency compensation when a hosted plugin changes its own
//! latency.
//!
//! # The gap this closes
//!
//! A plugin may change its latency while loaded — a linear-phase EQ switched
//! into an oversampling mode, a look-ahead limiter given a longer window. Every
//! format signals it (VST3 `restartComponent(kLatencyChanged)`, CLAP
//! `clap_host_latency.changed`, AU a `kAudioUnitProperty_Latency` property
//! change), and the engine carries all three to the plugin's latency cell.
//!
//! The graph does not read that cell. A node declares its latency in its
//! `Shape`, which the editor reads at insert; after that the editor's figure
//! is the one PDC compiles against until something hands it another
//! (`Editor::set_latency`): a latency change is a `Shape` change in the next
//! commit. Without this system every compensation delay keeps the
//! figure it was planned against — a plugin whose own latency is right and
//! whose *alignment against every other path* is wrong, audible as a track
//! drifting out of time with the rest of the mix.
//!
//! # One number, compared where it lives
//!
//! The poll compares the figure the plugin's node declares now
//! (`PluginControls::declared_latency`: its own latency plus the chunk its
//! pipeline holds, the same sum its `Shape` reports) with the one the editor
//! holds (`AudioGraphRes::node_latency`), and hands the first to the second
//! when they differ. There is no record of "what compensation was last planned
//! against" beside the editor's own figure, because the editor's figure *is*
//! that record.
//!
//! # Why polling rather than the invalidation callback
//!
//! `PluginHandle::on_invalidate` exists and fires `PluginInvalidation::Latency`
//! from the bridge thread. A callback cannot touch the `World` — it would need a
//! channel and a drain system, which is a second route for a value the plugin's
//! controls already hold. Polling reads the one owner. This is the same shape
//! as [`plugin_health_poll`](super::health::plugin_health_poll), which polls the
//! crash flag rather than subscribing to a death notification.
//!
//! # What the tests here do not cover
//!
//! `PluginClient::new` launches a plugin-server subprocess, so no unit test
//! here can put a real plugin in a graph. The tests below cover the two guards
//! that must *not* fire — a non-plugin node and a missing graph.
//! `tests/plugin_capture.rs` puts a real plugin behind the poll and watches a
//! latency change reach the graph.

use bevy_ecs::prelude::*;

use tutti_core::AudioNode;

use crate::graph::AudioGraphRes;
use crate::plugin_host::PluginShadow;

/// Hands the graph a plugin's latency when the figure its node declares no
/// longer matches the one the editor holds.
///
/// `AudioGraphRes::refresh_node_latency` feeds `Editor::set_latency`, and
/// marks the session edited, so the next commit moves PDC to the new figure without
/// touching the plugin — which is why this must run before the `Commit` phase
/// to be seen in the same frame. Pinned to the main thread
/// ([`NonSendMarker`](bevy_ecs::system::NonSendMarker)) for the reason
/// `commit_graph` is: `Editor::set_latency` collects what the audio thread
/// retired, which can be a plugin node whose drop tears down an editor window.
///
/// Runs over [`PluginShadow`], because the latency cell belongs to the node
/// and not to the handle. An entity with no shadow — a node that is not a
/// hosted plugin — is skipped, and so is one whose shadow was captured for a
/// node it no longer carries: the same guard `plugin_host::bind` uses.
///
/// Reads the graph before writing it, so a frame on which no plugin's latency
/// moved leaves `AudioGraphRes` unchanged.
pub fn plugin_latency_poll(
    _main: bevy_ecs::system::NonSendMarker,
    graph: Option<ResMut<AudioGraphRes>>,
    plugins: Query<(&AudioNode, &PluginShadow)>,
) {
    let Some(mut graph) = graph else {
        return;
    };

    for (node, shadow) in plugins.iter() {
        let Some(controls) = shadow.controls_for(node) else {
            continue;
        };
        // `refresh_node_latency` clamps the figure and compares it with the
        // editor's itself; the resource is marked changed only when it moved.
        if graph
            .bypass_change_detection()
            .refresh_node_latency(*node, controls.declared_latency())
        {
            graph.set_changed();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::AudioGraphRes;
    use crate::AudioEngineState;
    use bevy_app::prelude::*;

    /// The poll alone, with no commit: an edit it made would stay on the
    /// session's edit flag, and nothing else in this app edits the graph, so
    /// the observation is attributable.
    fn test_app() -> App {
        let mut app = App::new();
        app.insert_resource(AudioGraphRes::headless(0, 2));
        app.insert_resource(AudioEngineState::Running);
        app.add_systems(Update, plugin_latency_poll);
        app
    }

    /// An entity whose `AudioNode` is not a hosted plugin must not edit the
    /// session — every node in the graph carries `AudioNode`, so a poll that
    /// did not require a shadow would mark the graph edited every frame
    /// forever and
    /// re-run compensation on a graph nothing had changed.
    #[test]
    fn a_non_plugin_node_never_marks_the_graph_edited() {
        let mut app = test_app();
        let node = {
            let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
            let (node, controls) = graph.insert(tutti_nodes::testing::Const::mono(0.0));
            // The insert is an edit: send it, so the flag starts clear.
            assert!(graph.commit(), "the insert commits");
            (node, controls)
        };
        app.world_mut().spawn(node);

        app.update();

        assert!(
            !app.world().resource::<AudioGraphRes>().is_edited(),
            "a dc node is not a plugin and has no latency to compensate"
        );
    }

    /// With no graph resource the system is inert rather than panicking.
    ///
    /// `AudioGraphRes` belongs to `engine::build_into`, not this module, so
    /// a host that adds the plugin host alone must not crash.
    ///
    /// The property is "does not panic without a graph": the test passes by
    /// returning from `update`. There is nothing else to observe. Before the
    /// session, a `GraphDirty` resource could be checked for staying clean,
    /// but the edit flag lives in the graph, and here there is no graph.
    ///
    /// Mutation (run): the poll taking `ResMut<AudioGraphRes>` (not an
    /// `Option`) → Bevy panics on the missing resource → fails.
    #[test]
    fn the_poll_is_inert_without_a_graph() {
        let mut app = App::new();
        app.add_systems(Update, plugin_latency_poll);
        app.update();
    }
}
