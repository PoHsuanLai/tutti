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
//! ```rust
//! use bevy_app::prelude::*;
//! use bevy_tutti::prelude::*;
//!
//! let mut app = App::new();
//! // Ordinary Bevy prerequisites for the subsystems, not tutti's own; a real
//! // host has both from `DefaultPlugins`.
//! app.add_plugins((bevy_app::TaskPoolPlugin::default(), bevy_asset::AssetPlugin::default()));
//! // `disabled` only so this opens no device; a real host drops that field and
//! // the two `add_plugins` lines are unchanged.
//! app.add_plugins(TuttiPlugin { disabled: true, ..Default::default() })
//!     .add_plugins(LatencyCompensationPlugin);
//! app.update();
//!
//! // Both resources exist from the first frame. `GraphLatency` is the figure a
//! // DAW displays; zero until some node in the graph reports latency, which is
//! // the common case and why this plugin is opt-in.
//! assert!(app.world().get_resource::<ChannelCompensation>().is_some());
//! assert!(app.world().resource::<GraphLatency>().is_empty());
//! ```
//!
//! # Ordering
//!
//! The system runs in [`GraphReconcileSystems::Compensate`], between `Despawn`
//! and `Commit` — after the frame's graph edits are in, before they're published
//! to the audio thread. An app that schedules its own graph mutation must order
//! it before that set, or its nodes miss the frame's compensation:
//!
//! ```rust
//! use bevy_app::prelude::*;
//! use bevy_ecs::prelude::*;
//! use bevy_tutti::prelude::*;
//! use tutti_core::dsp::Net;
//! use tutti_core::Hz;
//! use tutti_nodes::testing::Osc;
//!
//! /// A host system that edits the graph itself.
//! fn my_graph_edits(mut graph: ResMut<AudioGraphRes>, mut dirty: ResMut<GraphDirty>) {
//!     graph.0.add(Osc::sine(Hz(440.0)));
//!     // Say so, or the compensation pass skips the frame entirely.
//!     dirty.0 = true;
//! }
//!
//! let mut app = App::new();
//! app.insert_resource(AudioGraphRes(Net::with_backend(2)));
//! app.insert_resource(AudioEngineState::Running);
//! app.add_plugins((GraphReconcilePlugin, LatencyCompensationPlugin));
//! app.add_systems(Update, my_graph_edits.before(GraphReconcileSystems::Compensate));
//! app.update();
//!
//! // The frame's edit was in before compensation ran, so `commit_graph`
//! // cleared the flag on the way past.
//! assert!(!app.world().resource::<GraphDirty>().0);
//! ```
//!
//! # Sources outside the graph
//!
//! A node inside the graph can be delayed; a file being streamed from disk
//! cannot. Such a source instead seeks its read head *earlier* so its audio
//! arrives already aligned. [`ChannelCompensation`] carries the per-channel
//! figures for that, republished every time compensation runs, and the sampler
//! subscribes to it at engine build.
//!
//! # Reporting without applying
//!
//! This plugin *applies* compensation, inserting delays. A host that only wants
//! to know what a graph would need — a latency readout that must not perturb the
//! graph — calls [`tutti_core::latency::plan`] on `AudioGraphRes.0` directly;
//! `Net` implements the `LatencyGraph` trait it takes. There is no wrapper here
//! because there would be nothing to wrap: `plan` needs no ECS state and mutates
//! nothing, so a wrapper would be a rename.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use std::sync::Arc;

use crate::graph::{engine_ready, AudioGraphRes, GraphDirty, GraphReconcileSystems};
use tutti_core::RtPublish;
use tutti_core::{latency, Samples};

/// Per-output-channel pre-roll for sources outside the audio graph.
///
/// Cloneable subscription to the table [`compensate_graph`] publishes. The
/// sampler holds one of these; a host wanting to display or apply the figures
/// elsewhere can clone it from the resource.
///
/// Read a channel with `compensation.0.read().get(channel)`. A `for_channel`
/// convenience lived here and was deleted: one line over an expression
/// [`RtPublish::read`] already spells is surface without capability.
#[derive(Resource, Clone, Default)]
pub struct ChannelCompensation(pub Arc<RtPublish<Vec<Samples>>>);

/// The graph's total latency, as of the last compensation run.
///
/// The figure a DAW displays as "latency: N samples", and the one a host offsets
/// recording by. Distinct from [`ChannelCompensation`], which answers "how far
/// must *this* source pre-roll": the worst-case path is often the channel whose
/// pre-roll is zero, so the total is **not recoverable** from the per-channel
/// table — it was computed here and thrown away for as long as this resource
/// did not exist.
///
/// Zero when no node in the graph reports latency, which is the common case.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GraphLatency(pub Samples);

impl GraphLatency {
    /// Whether any path in the graph needs compensation at all.
    pub fn is_empty(&self) -> bool {
        self.0 == Samples(0)
    }
}

/// Adds latency compensation to the graph reconcile pipeline.
///
/// See the [module docs](self) for ordering and for how out-of-graph sources
/// are handled.
pub struct LatencyCompensationPlugin;

impl Plugin for LatencyCompensationPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ChannelCompensation>()
            .init_resource::<GraphLatency>()
            .add_systems(
                Update,
                compensate_graph
                    .in_set(GraphReconcileSystems::Compensate)
                    .run_if(engine_ready),
            );
    }
}

/// Aligns every path in the graph, then republishes the per-channel table and
/// the graph's total latency.
///
/// Runs only when a reconcile system touched the graph this frame — the same
/// `GraphDirty` flag that gates the commit. It deliberately does **not** clear
/// the flag; `commit_graph` does that after publishing to the audio thread.
/// Both `graph` and `dirty` are optional, and for the same reason: neither
/// belongs to this plugin. `LatencyCompensationPlugin` is `pub` and documented
/// as opt-in, so a host can add it alone; `GraphDirty` is
/// `GraphReconcilePlugin`'s and `AudioGraphRes` is `engine::build_into`'s. The
/// `engine_ready` gate covers neither — it reads `AudioEngineState`, a value a
/// host can insert on its own. Nothing to compensate is a no-op.
pub fn compensate_graph(
    graph: Option<ResMut<AudioGraphRes>>,
    dirty: Option<Res<GraphDirty>>,
    published: Res<ChannelCompensation>,
    mut total: ResMut<GraphLatency>,
) {
    let (Some(mut graph), Some(dirty)) = (graph, dirty) else {
        return;
    };
    if !dirty.0 {
        return;
    }

    let compensation = latency::compensate(&mut graph.0);
    total.0 = compensation.total();
    published
        .0
        .publish(Arc::new(compensation.channels().to_vec()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_core::dsp::Net;
    use tutti_core::dsp::Source;
    use tutti_core::{ChannelLayout, Db};
    use tutti_nodes::testing::Const;
    use tutti_nodes::LimiterNode;

    /// App with the graph resource + dirty flag, but no reconcile pipeline —
    /// enough to drive the compensation system directly.
    ///
    /// `AudioEngineState::Running` stands in for a built engine: the
    /// compensation system is gated on `engine_ready`, which reads the state
    /// rather than probing for the graph resource.
    fn test_app(graph: Net) -> App {
        let mut app = App::new();
        app.insert_resource(AudioGraphRes(graph));
        app.insert_resource(crate::AudioEngineState::Running);
        app.init_resource::<GraphDirty>();
        app.add_plugins(LatencyCompensationPlugin);
        app
    }

    /// ch0 through a limiter, ch1 dry — ch1's source must pre-roll to match.
    ///
    /// The limiter is the engine's own `LimiterNode`, whose lookahead is what
    /// it reports as latency — so the plan is exercised on the latency-bearing
    /// node the engine ships, not on fundsp's.
    fn skewed_graph() -> (Net, Samples) {
        let mut graph = Net::with_backend(2);
        let a = graph.add(Const::mono(1.0));
        let eff = graph.add(LimiterNode::with_channels(
            ChannelLayout::MONO,
            Db(-1.0),
            Db(-0.3),
        ));
        let b = graph.add(Const::mono(1.0));
        graph.connect(a, 0, eff, 0);
        graph.set_output_source(0, Source::Local(eff, 0));
        graph.set_output_source(1, Source::Local(b, 0));

        let lat = tutti_core::LatencyGraph::latency(&graph, eff);
        // Every test built on this graph compares against `lat`; if the
        // limiter ever reported no latency they would all agree on zero and
        // pass while testing nothing.
        assert!(lat.get() > 0, "LimiterNode must report its lookahead");
        (graph, lat)
    }

    #[test]
    fn publishes_the_table_when_the_graph_is_dirty() {
        let (graph, eff_lat) = skewed_graph();
        let mut app = test_app(graph);
        app.world_mut().resource_mut::<GraphDirty>().0 = true;

        app.update();

        let published = app.world().resource::<ChannelCompensation>();
        let table = published.0.read();
        assert_eq!(table.first().copied(), Some(Samples(0)));
        assert_eq!(table.get(1).copied(), Some(eff_lat));
    }

    /// The graph's *total* latency is published too — and it cannot be recovered
    /// from the per-channel table.
    ///
    /// In this graph the limiter defines the worst-case path, and the channel it
    /// feeds pre-rolls by zero: the figure a DAW displays is exactly the one the
    /// table does not contain. It was computed and discarded for as long as
    /// `GraphLatency` did not exist.
    #[test]
    fn publishes_the_graphs_total_latency_not_just_the_per_channel_table() {
        let (graph, eff_lat) = skewed_graph();
        let mut app = test_app(graph);
        app.world_mut().resource_mut::<GraphDirty>().0 = true;

        app.update();

        assert_eq!(app.world().resource::<GraphLatency>().0, eff_lat);
        assert!(!app.world().resource::<GraphLatency>().is_empty());

        // The point: reading it off the table gives the wrong answer.
        let table = app.world().resource::<ChannelCompensation>().0.read();
        assert_eq!(
            table.first().copied(),
            Some(Samples(0)),
            "the channel that defines the latency pre-rolls by zero"
        );
    }

    /// A graph where nothing reports latency has no figure to display.
    #[test]
    fn a_graph_with_no_latency_reports_none() {
        let mut graph = Net::with_backend(2);
        let a = graph.add(Const::mono(1.0));
        graph.set_output_source(0, Source::Local(a, 0));
        graph.set_output_source(1, Source::Local(a, 0));

        let mut app = test_app(graph);
        app.world_mut().resource_mut::<GraphDirty>().0 = true;
        app.update();

        assert!(app.world().resource::<GraphLatency>().is_empty());
        assert_eq!(app.world().resource::<GraphLatency>().0, Samples(0));
    }

    #[test]
    fn does_nothing_while_the_graph_is_clean() {
        let (graph, _) = skewed_graph();
        let mut app = test_app(graph);
        // GraphDirty defaults to false — no edits this frame.

        app.update();

        let published = app.world().resource::<ChannelCompensation>();
        assert!(published.0.read().is_empty(), "no table published");
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

    // `for_channel_is_zero_outside_the_table` was deleted with the `for_channel`
    // method it covered. Out-of-range now reads as `Vec::get -> None` at the call
    // site, which is std's guarantee rather than this crate's to test.
}
