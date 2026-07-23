//! Opt-in latency (plugin delay) compensation.
//!
//! Nodes like lookahead limiters, linear-phase EQs, and out-of-process plugins
//! can't produce output for sample *n* until they've seen samples past it. Where
//! two paths of unequal latency meet, the earlier one arrives too soon — audible
//! flam, comb filtering on parallel sends. Compensation delays the early paths
//! to match the late one.
//!
//! This costs a graph walk per commit, and a host that never loads a
//! latency-reporting node never needs it, so it is **not** part of
//! [`TuttiPlugin`](crate::TuttiPlugin). Add it explicitly:
//!
//! ```rust,ignore
//! App::new()
//!     .add_plugins(TuttiPlugin::default())
//!     .add_plugins(LatencyCompensationPlugin);
//! ```
//!
//! # Ordering
//!
//! The system runs in [`GraphReconcileSystems::Compensate`], between `Despawn`
//! and `Commit` — after the frame's graph edits are in, before they're published
//! to the audio thread. An app that schedules its own graph mutation must order
//! it before that set, or its nodes miss the frame's compensation:
//!
//! ```rust,ignore
//! app.add_systems(Update, my_graph_edits.before(GraphReconcileSystems::Compensate));
//! ```
//!
//! # Sources outside the graph
//!
//! A node inside the graph can be delayed; a file being streamed from disk
//! cannot. Such a source instead seeks its read head *earlier* so its audio
//! arrives already aligned. [`ChannelCompensation`] carries the per-channel
//! figures for that, republished every time compensation runs, and the sampler
//! subscribes to it at engine build.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use std::sync::Arc;

use arc_swap::ArcSwap;
use tutti_core::ecs::{engine_ready, AudioGraphRes, GraphDirty, GraphReconcileSystems};
use tutti_core::{latency, Samples};

/// Per-output-channel pre-roll for sources outside the audio graph.
///
/// Cloneable subscription to the table [`compensate_graph`] publishes. The
/// sampler holds one of these; a host wanting to display or apply the figures
/// elsewhere can clone it from the resource.
#[derive(Resource, Clone, Default)]
pub struct ChannelCompensation(pub Arc<ArcSwap<Vec<Samples>>>);

impl ChannelCompensation {
    /// Pre-roll for a source feeding `channel`. Zero if uncompensated.
    pub fn for_channel(&self, channel: usize) -> Samples {
        self.0.load().get(channel).copied().unwrap_or_default()
    }
}

/// Adds latency compensation to the graph reconcile pipeline.
///
/// See the [module docs](self) for ordering and for how out-of-graph sources
/// are handled.
pub struct LatencyCompensationPlugin;

impl Plugin for LatencyCompensationPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ChannelCompensation>().add_systems(
            Update,
            compensate_graph
                .in_set(GraphReconcileSystems::Compensate)
                .run_if(engine_ready),
        );
    }
}

/// Aligns every path in the graph, then republishes the per-channel table.
///
/// Runs only when a reconcile system touched the graph this frame — the same
/// `GraphDirty` flag that gates the commit. It deliberately does **not** clear
/// the flag; `commit_graph` does that after publishing to the audio thread.
pub fn compensate_graph(
    mut graph: ResMut<AudioGraphRes>,
    dirty: Res<GraphDirty>,
    published: Res<ChannelCompensation>,
) {
    if !dirty.0 {
        return;
    }

    let compensation = latency::compensate(&mut graph.0);
    published
        .0
        .store(Arc::new(compensation.channels().to_vec()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_core::dsp::Net;
    use tutti_core::dsp::{dc, limiter};
    use tutti_core::Source;

    /// App with the graph resource + dirty flag, but no reconcile pipeline —
    /// enough to drive the compensation system directly.
    fn test_app(graph: Net) -> App {
        let mut app = App::new();
        app.insert_resource(AudioGraphRes(graph));
        app.init_resource::<GraphDirty>();
        app.add_plugins(LatencyCompensationPlugin);
        app
    }

    /// ch0 through a limiter, ch1 dry — ch1's source must pre-roll to match.
    fn skewed_graph() -> (Net, Samples) {
        let mut graph = Net::with_backend(2);
        let a = graph.add(dc(1.0));
        let eff = graph.add(limiter(0.01, 0.01));
        let b = graph.add(dc(1.0));
        graph.connect(a, 0, eff, 0);
        graph.set_output_source(0, Source::Local(eff, 0));
        graph.set_output_source(1, Source::Local(b, 0));

        let lat = tutti_core::LatencyGraph::latency(&graph, eff);
        (graph, lat)
    }

    #[test]
    fn publishes_the_table_when_the_graph_is_dirty() {
        let (graph, eff_lat) = skewed_graph();
        let mut app = test_app(graph);
        app.world_mut().resource_mut::<GraphDirty>().0 = true;

        app.update();

        let published = app.world().resource::<ChannelCompensation>();
        assert_eq!(published.for_channel(0), Samples(0));
        assert_eq!(published.for_channel(1), eff_lat);
    }

    #[test]
    fn does_nothing_while_the_graph_is_clean() {
        let (graph, _) = skewed_graph();
        let mut app = test_app(graph);
        // GraphDirty defaults to false — no edits this frame.

        app.update();

        let published = app.world().resource::<ChannelCompensation>();
        assert!(published.0.load().is_empty(), "no table published");
    }

    #[test]
    fn leaves_the_dirty_flag_for_commit_to_clear() {
        let (graph, _) = skewed_graph();
        let mut app = test_app(graph);
        app.world_mut().resource_mut::<GraphDirty>().0 = true;

        app.update();

        assert!(
            app.world().resource::<GraphDirty>().0,
            "commit_graph owns clearing the flag"
        );
    }

    #[test]
    fn for_channel_is_zero_outside_the_table() {
        let (graph, _) = skewed_graph();
        let mut app = test_app(graph);
        app.world_mut().resource_mut::<GraphDirty>().0 = true;

        app.update();

        assert_eq!(
            app.world()
                .resource::<ChannelCompensation>()
                .for_channel(99),
            Samples(0)
        );
    }
}
