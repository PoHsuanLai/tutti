//! The graph's latency (plugin delay) compensation figures.
//!
//! Nodes like lookahead limiters, linear-phase EQs, and out-of-process plugins
//! can't produce output for sample *n* until they've seen samples past it. Where
//! two paths of unequal latency meet, the earlier one arrives too soon — audible
//! flam, comb filtering on parallel sends. Compensation delays the early paths
//! to match the late one.
//!
//! **The graph always compensates**: every commit's plan delays the early
//! paths inside the graph. And the figures of every plan are published, by
//! [`commit_graph`](crate::graph::commit_graph) as it sends the plan, on every
//! graph: the per-channel pre-roll ([`ChannelCompensation`]) a disk-streamed
//! source seeks by, and the total ([`GraphLatency`]) a DAW displays. Both
//! resources exist whenever [`GraphReconcilePlugin`](crate::graph::GraphReconcilePlugin)
//! does. A graph that compensated without publishing would leave a disk
//! source pre-rolling by nothing against a delayed graph, so no host opts
//! out of the figures.
//!
//! [`LatencyCompensationPlugin`] is optional and adds only a check: in debug
//! builds, on every frame that edits the graph, that the latency fold over the
//! whole graph spec ([`AudioGraphRes::latency_plan`], what a latency readout
//! reads: audio edges plus each node's event and param-modulation sources)
//! agrees with what the compiled plan compensates by. It applies nothing and
//! publishes nothing; the graph compiler and the commit do both.
//!
//! ```rust
//! use bevy_app::prelude::*;
//! use bevy_tutti::prelude::*;
//!
//! let mut app = App::new();
//! // Ordinary Bevy prerequisites for the subsystems, not tutti's own; a real
//! // host has both from `DefaultPlugins`.
//! app.add_plugins((bevy_app::TaskPoolPlugin::default(), bevy_asset::AssetPlugin::default()));
//! // `disabled` only so this opens no device; a real host drops that field.
//! app.add_plugins(TuttiPlugin { disabled: true, ..Default::default() });
//! app.update();
//!
//! // Both resources exist from the first frame, without
//! // `LatencyCompensationPlugin`. `GraphLatency` is the figure a DAW displays;
//! // zero until some node in the graph reports latency.
//! assert!(app.world().get_resource::<ChannelCompensation>().is_some());
//! assert!(app.world().resource::<GraphLatency>().is_empty());
//! ```
//!
//! # Ordering
//!
//! The figures are published in [`GraphReconcileSystems::Commit`], with the
//! plan. An app that schedules its own graph mutation must order it before
//! that set, or its edit misses the frame's commit:
//!
//! ```rust
//! use bevy_app::prelude::*;
//! use bevy_ecs::prelude::*;
//! use bevy_tutti::prelude::*;
//! use tutti_core::Hz;
//! use tutti_nodes::testing::Osc;
//!
//! /// A host system that edits the graph itself. Every edit made through
//! /// `AudioGraphRes` marks the session edited; the frame's commit sends it.
//! fn my_graph_edits(mut graph: ResMut<AudioGraphRes>) {
//!     graph.insert(Osc::sine(Hz(440.0)));
//! }
//!
//! let mut app = App::new();
//! app.insert_resource(AudioGraphRes::headless(0, 2));
//! app.insert_resource(AudioEngineState::Running);
//! app.add_plugins(GraphReconcilePlugin);
//! app.add_systems(Update, my_graph_edits.before(GraphReconcileSystems::Commit));
//! app.update();
//!
//! // The frame's edit was in before the commit ran, so `commit_graph` sent
//! // it and the session is clean again.
//! assert!(!app.world().resource::<AudioGraphRes>().is_edited());
//! ```
//!
//! # Sources outside the graph
//!
//! A node inside the graph can be delayed; a file being streamed from disk
//! cannot. Such a source instead seeks its read head *earlier* so its audio
//! arrives already aligned. [`ChannelCompensation`] carries the per-channel
//! figures for that, republished with every plan sent, and the sampler
//! subscribes to it at engine build.
//!
//! # Asking between commits
//!
//! A host that wants the figure for the graph as edited but not yet committed
//! — a latency readout — calls [`AudioGraphRes::latency_plan`] directly: it
//! needs no ECS state and mutates nothing.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use std::sync::Arc;

use crate::graph::{engine_ready, AudioGraphRes, GraphReconcileSystems};
use tutti_core::{ArcLend, Reader, Samples};

/// Per-output-channel pre-roll for sources outside the audio graph.
///
/// Cloneable subscription to the table
/// [`commit_graph`](crate::graph::commit_graph) publishes with every plan it
/// sends. The sampler holds one of these; a host wanting to display or apply
/// the figures elsewhere can clone it from the resource.
///
/// To read it, register a [`Reader`] with [`reader`](Self::reader), keep it
/// where the reading code runs (a system's `Local`, a struct field), and
/// read a channel with `reader.lend().get(channel)`, which is `None` for a
/// channel past the end of the table. The table is empty until the first
/// plan is sent.
///
/// The resource can be rebound to another cell (once, when the graph's own
/// replaces the one a plugin initialized before it was inserted): a reader
/// registered before that keeps reading the old cell. Register again when
/// the resource changes (`Res::is_changed`), or compare
/// [`Reader::cell`] with `.0`.
#[derive(Resource, Clone, Default)]
pub struct ChannelCompensation(pub Arc<ArcLend<Vec<Samples>>>);

impl ChannelCompensation {
    /// A reader of the table: one per reading thread, registered off the
    /// audio thread (it takes the cell's publisher lock and may allocate),
    /// kept, and lent through for each read.
    pub fn reader(&self) -> Reader<Vec<Samples>> {
        self.0.reader()
    }
}

/// The graph's total latency, as of the last plan sent to the audio thread.
///
/// The figure a DAW displays as "latency: N samples", and the one a host offsets
/// recording by. Distinct from [`ChannelCompensation`], which answers "how far
/// must *this* source pre-roll": the worst-case path is often the channel whose
/// pre-roll is zero, so the total is **not recoverable** from the per-channel
/// table.
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

/// Mirror the session's published figures into the world: the
/// [`ChannelCompensation`] resource holds the session's own cell (rebound
/// once, when it is another, as one a reconcile plugin initialized before
/// the graph was inserted), and [`GraphLatency`] its total, marked changed
/// only when it moved.
pub(crate) fn mirror(
    graph: &AudioGraphRes,
    table: Option<ResMut<ChannelCompensation>>,
    total: Option<ResMut<GraphLatency>>,
) {
    if let Some(mut table) = table {
        let cell = graph.compensation();
        if !Arc::ptr_eq(&table.0, &cell) {
            table.0 = cell;
        }
    }
    if let Some(mut total) = total {
        total.set_if_neq(GraphLatency(graph.latency()));
    }
}

/// [`mirror`], for a caller holding the world (a device restart).
pub(crate) fn mirror_world(world: &mut World) {
    world.resource_scope(|world, graph: Mut<AudioGraphRes>| {
        let mut state: bevy_ecs::system::SystemState<(
            Option<ResMut<ChannelCompensation>>,
            Option<ResMut<GraphLatency>>,
        )> = bevy_ecs::system::SystemState::new(world);
        let Ok((table, total)) = state.get_mut(world) else {
            return;
        };
        mirror(&graph, table, total);
    });
}

/// Adds [`compensate_graph`], a debug-build check of the graph's latency
/// compensation, in [`GraphReconcileSystems::Compensate`].
///
/// Optional: [`TuttiPlugin`](crate::TuttiPlugin) does not add it, and the
/// compensation figures are published without it. See the
/// [module docs](self).
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

/// Checks, in debug builds, that the graph's latency fold matches the plan the
/// frame's commit will compile.
///
/// Runs only on a frame that edits the graph (the session's edit flag set).
/// The fold
/// over the whole spec ([`AudioGraphRes::latency_plan`]: audio edges plus
/// event and param-modulation sources) must equal what the compiled plan
/// compensates by. Both are read on the control side from the one spec, so a
/// disagreement is a bug, not a race.
///
/// # Panics
///
/// In debug builds, when the two disagree. Publishes nothing
/// ([`commit_graph`](crate::graph::commit_graph) does), and in release builds
/// does nothing. Returns quietly while either resource is missing.
pub fn compensate_graph(graph: Option<Res<AudioGraphRes>>) {
    let Some(graph) = graph else {
        return;
    };
    if !cfg!(debug_assertions) || !graph.is_edited() {
        return;
    }
    // `None` only for a spec that does not compile: its commit refuses it
    // too, and there is no plan to check against.
    let Some((channels, total)) = graph.compensate() else {
        return;
    };
    let folded = graph.latency_plan();
    // The fold reports no channels at all when nothing is latent (its "empty
    // result"); the plan has one zero per output. Same figures.
    let width = folded.channels().len().max(channels.len());
    let per_channel = |c: &[Samples]| -> Vec<Samples> {
        (0..width)
            .map(|i| c.get(i).copied().unwrap_or(Samples(0)))
            .collect()
    };
    debug_assert_eq!(
        (per_channel(folded.channels()), folded.total()),
        (per_channel(&channels), total),
        "the spec's latency fold and the compiled plan disagree"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::GraphSource;
    use tutti_core::{ChannelLayout, Db};
    use tutti_nodes::testing::Const;
    use tutti_nodes::LimiterNode;

    /// App with the graph resource and the reconcile pipeline, and **without**
    /// `LatencyCompensationPlugin`: the figures are `commit_graph`'s to
    /// publish, on every graph.
    ///
    /// `AudioEngineState::Running` stands in for a built engine: the
    /// reconcile systems are gated on `engine_ready`, which reads the state
    /// rather than probing for the graph resource.
    fn test_app(graph: AudioGraphRes) -> App {
        let mut app = App::new();
        app.insert_resource(graph);
        app.insert_resource(crate::AudioEngineState::Running);
        app.add_plugins(crate::graph::GraphReconcilePlugin);
        app
    }

    /// App with the graph and `LatencyCompensationPlugin`'s check alone, no
    /// commit: to drive that one system.
    fn check_app(graph: AudioGraphRes) -> App {
        let mut app = App::new();
        app.insert_resource(graph);
        app.insert_resource(crate::AudioEngineState::Running);
        app.add_plugins(LatencyCompensationPlugin);
        app
    }

    /// ch0 through a limiter, ch1 dry — ch1's source must pre-roll to match.
    ///
    /// The limiter is the engine's own `LimiterNode`, whose lookahead is what
    /// it reports as latency — so the plan is exercised on the latency-bearing
    /// node the engine ships.
    fn skewed_graph() -> (AudioGraphRes, Samples) {
        let mut graph = AudioGraphRes::headless(0, 2);
        let (a, _) = graph.insert(Const::mono(1.0));
        let (eff, _) = graph.insert(LimiterNode::with_channels(
            ChannelLayout::MONO,
            Db(-1.0),
            Db(-0.3),
        ));
        let (b, _) = graph.insert(Const::mono(1.0));
        graph.set_source(eff, 0, GraphSource::Node(a, 0));
        graph.set_output_source(0, GraphSource::Node(eff, 0));
        graph.set_output_source(1, GraphSource::Node(b, 0));

        let lat = graph.node_latency(eff);
        // Every test built on this graph compares against `lat`; if the
        // limiter ever reported no latency they would all agree on zero and
        // pass while testing nothing.
        assert!(lat.get() > 0, "LimiterNode must report its lookahead");
        (graph, lat)
    }

    #[test]
    fn publishes_the_table_when_the_graph_is_edited() {
        let (graph, eff_lat) = skewed_graph();
        let mut app = test_app(graph);

        app.update();

        let published = app.world().resource::<ChannelCompensation>();
        let mut reader = published.reader();
        let table = reader.lend();
        assert_eq!(table.first().copied(), Some(Samples(0)));
        assert_eq!(table.get(1).copied(), Some(eff_lat));
    }

    /// The graph's *total* latency is published too — and it cannot be recovered
    /// from the per-channel table.
    ///
    /// In this graph the limiter defines the worst-case path, and the channel it
    /// feeds pre-rolls by zero: the figure a DAW displays is exactly the one the
    /// table does not contain.
    #[test]
    fn publishes_the_graphs_total_latency_not_just_the_per_channel_table() {
        let (graph, eff_lat) = skewed_graph();
        let mut app = test_app(graph);

        app.update();

        assert_eq!(app.world().resource::<GraphLatency>().0, eff_lat);
        assert!(!app.world().resource::<GraphLatency>().is_empty());

        // The point: reading it off the table gives the wrong answer.
        let mut reader = app.world().resource::<ChannelCompensation>().reader();
        let table = reader.lend();
        assert_eq!(
            table.first().copied(),
            Some(Samples(0)),
            "the channel that defines the latency pre-rolls by zero"
        );
    }

    /// A graph where nothing reports latency has no figure to display.
    #[test]
    fn a_graph_with_no_latency_reports_none() {
        let mut graph = AudioGraphRes::headless(0, 2);
        let (a, _) = graph.insert(Const::mono(1.0));
        graph.set_output_source(0, GraphSource::Node(a, 0));
        graph.set_output_source(1, GraphSource::Node(a, 0));

        let mut app = test_app(graph);
        app.update();

        assert!(app.world().resource::<GraphLatency>().is_empty());
        assert_eq!(app.world().resource::<GraphLatency>().0, Samples(0));
    }

    /// **A frame with no edit publishes nothing**: the figures go out with a
    /// plan, and a clean session sends none. The graph's own inserts edit it,
    /// so the first frame commits; a sentinel published into the session's
    /// cell after that must survive a frame with no edit.
    ///
    /// (Before the session, this read "`GraphDirty` false → no table"; the
    /// flag is gone, and every edit made through `AudioGraphRes` is sent.)
    ///
    /// Mutation (run): the session's `Commit` phase committing whether or not
    /// the graph is edited → the second frame republishes the plan's table
    /// over the sentinel → fails.
    #[test]
    fn does_nothing_while_the_graph_is_clean() {
        let (graph, _) = skewed_graph();
        let mut app = test_app(graph);
        app.update();
        assert!(!app.world().resource::<AudioGraphRes>().is_edited());
        let sentinel = vec![Samples(99)];
        app.world()
            .resource::<ChannelCompensation>()
            .0
            .publish(std::sync::Arc::new(sentinel.clone()));

        app.update();

        let published = app.world().resource::<ChannelCompensation>();
        assert_eq!(*published.reader().lend(), sentinel, "no table published");
    }

    /// **A frame's `Reclaim` phase frees a table a block held across the
    /// commit that replaced it**, on a later frame with no edit (so no
    /// publish): the adapter schedules the phase every frame. A held `Loan`
    /// stands in for the block.
    ///
    /// Mutation (run): `reclaim_graph` not running the session's phase →
    /// the old table is still retired, fails.
    ///
    /// Which cells the phase sweeps is the session's, pinned in
    /// `tutti-session`'s tests (the nodes' reclaimers in `tests/graph.rs`);
    /// this pins only that the adapter runs it.
    #[test]
    fn a_frame_frees_a_table_a_block_held_across_its_replacement() {
        let (graph, _) = skewed_graph();
        let mut app = test_app(graph);
        app.update();
        let table = app.world().resource::<ChannelCompensation>().0.clone();
        let mut reader = table.reader();
        {
            let _held = reader.lend();
            app.world_mut()
                .resource_mut::<AudioGraphRes>()
                .insert(Const::mono(0.5));
            // The commit replaces the table under the loan; this frame's
            // reclaim leaves it.
            app.update();
        }
        assert_eq!(table.retired_len(), 1, "retired, held across the commit");
        app.update();
        assert!(!app.world().resource::<AudioGraphRes>().is_edited());
        assert_eq!(table.retired_len(), 0, "freed by the frame's reclaim");
    }

    /// The check reads the edit flag; it never clears it.
    ///
    /// Mutation (run): `compensate_graph` committing the graph after its
    /// check → the flag clears → fails.
    #[test]
    fn the_check_leaves_the_edit_flag_for_commit_to_clear() {
        let (graph, _) = skewed_graph();
        let mut app = check_app(graph);
        assert!(app.world().resource::<AudioGraphRes>().is_edited());

        app.update();

        assert!(
            app.world().resource::<AudioGraphRes>().is_edited(),
            "the session's Commit phase owns clearing the flag"
        );
    }

    /// **The check agrees on a latent graph, and on one with no latency**:
    /// the spec's fold (`latency_plan`, what a readout reads) is what the
    /// compiled plan compensates by, zeros and all (the fold reports no
    /// channels when nothing is latent; the plan a zero per output).
    ///
    /// Mutation (run): the session's `planned_compensation` returning the
    /// plan's channels reversed → the check panics on the skewed graph.
    #[cfg(debug_assertions)]
    #[test]
    fn the_check_holds_on_latent_and_latency_free_graphs() {
        let (graph, _) = skewed_graph();
        let mut app = check_app(graph);
        app.update();

        let mut graph = AudioGraphRes::headless(0, 2);
        let (a, _) = graph.insert(Const::mono(1.0));
        graph.set_outputs_from(a);
        let mut app = check_app(graph);
        app.update();
    }

    /// **The published figures are the plan's**, with no
    /// `LatencyCompensationPlugin`: what `commit_graph` publishes is exactly
    /// the `Plan::compensation` / `total_latency` the commit sent the
    /// executor, so a pre-roll the sampler reads cannot drift off the delay
    /// the graph applies, and a graph without `LatencyCompensationPlugin`
    /// still publishes.
    ///
    /// Mutation (run): the session's `publish_sent` publishing the channels
    /// reversed → the table and the sent plan disagree on both channels (and
    /// the two publish tests above fail); `latency::mirror` not rebinding
    /// the resource to the session's cell → the plugin's empty table stays
    /// → fails.
    #[test]
    fn publishes_the_compensation_its_commit_sends() {
        let (graph, eff_lat) = skewed_graph();
        let authored = [graph.output_source(0), graph.output_source(1)];
        let mut app = test_app(graph);
        app.update();
        assert!(
            !app.world().resource::<AudioGraphRes>().is_edited(),
            "committed"
        );

        let (sent_channels, sent_total) = app
            .world()
            .resource::<AudioGraphRes>()
            .sent_compensation()
            .expect("the commit sent a plan");
        let mut reader = app.world().resource::<ChannelCompensation>().reader();
        let table = reader.lend();
        assert_eq!(&*table, &sent_channels[..], "per-channel pre-roll");
        assert_eq!(
            app.world().resource::<GraphLatency>().0,
            sent_total,
            "total latency"
        );
        assert_eq!(sent_total, eff_lat, "and it is the limiter's lookahead");
        // Nothing was spliced in to get there: the compiler compensates, and
        // both channels still read the nodes they were wired to.
        let graph = app.world().resource::<AudioGraphRes>();
        assert_eq!(
            [graph.output_source(0), graph.output_source(1)],
            authored,
            "nothing is spliced into the wiring"
        );
    }

    /// **A re-prepare to a new rate republishes the figures once it
    /// resumes**, with nothing else marking the graph dirty: the limiter's
    /// lookahead is a time, so its latency in samples moves with the rate,
    /// and the resumed plan compensates by the new figure: `commit_graph`
    /// publishes the resumed plan's figures on the frame its collect resumes
    /// it.
    ///
    /// Mutation (run): the session's `Commit` phase not publishing on a
    /// resume (`publish_sent` skipped) → the 44.1 kHz figure stays
    /// published → fails.
    #[test]
    fn a_re_prepare_republishes_the_figures_once_it_resumes() {
        let mut graph = AudioGraphRes::headless(0, 2);
        let (a, _) = graph.insert(Const::mono(1.0));
        let (eff, _) = graph.insert(LimiterNode::with_channels(
            ChannelLayout::MONO,
            Db(-1.0),
            Db(-0.3),
        ));
        graph.set_source(eff, 0, GraphSource::Node(a, 0));
        graph.set_output_source(0, GraphSource::Node(eff, 0));
        graph.set_output_source(1, GraphSource::Node(a, 0));
        let at_44k = graph.node_latency(eff);
        let mut side = graph.take_audio_side();
        let mut app = test_app(graph);
        app.update();
        assert_eq!(app.world().resource::<GraphLatency>().0, at_44k);
        let mut out = [0.0f32; 2];
        side.tick(&[], &mut out);

        app.world_mut()
            .resource_mut::<AudioGraphRes>()
            .set_sample_rate(tutti_core::SampleRate(48_000.0));
        for _ in 0..3 {
            side.tick(&[], &mut out);
            app.update();
        }
        let at_48k = app.world().resource::<AudioGraphRes>().node_latency(eff);
        assert_ne!(at_48k, at_44k, "the lookahead moved in samples");
        assert_eq!(app.world().resource::<GraphLatency>().0, at_48k);
        assert_eq!(
            app.world()
                .resource::<ChannelCompensation>()
                .reader()
                .lend()
                .get(1),
            Some(&at_48k),
            "the dry channel pre-rolls by the new figure"
        );
    }
}
