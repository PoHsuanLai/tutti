//! What an export renders: the graph.
//!
//! An export renders a `tutti_graph` editor/executor pair ([`RenderGraph`]) and
//! nothing else. It becomes a frame source (`render::driver`), and the gate,
//! resample, dither and encoders downstream see only frames.

use std::sync::Arc;
use tutti_types::SampleRate;

use tutti_graph::GraphTail;
use tutti_graph::{Editor, Executor, Pool, Prepare};
use tutti_types::{ChannelTopology, Samples};

use crate::{ConfigError, Result};

/// The largest block, in frames, a graph is prepared for in an export
/// ([`RenderGraph::prepare`]).
///
/// 1024 frames: long enough that the executor's per-block walk is paid rarely
/// on a long bounce, short enough that a block's planes stay in cache.
// A multiple of 64, from when the sampler's clip readers rendered 64-frame
// pieces from each block's start: the golden digests `tests/graph_source.rs`
// pins were recorded with those pieces. The readers now render each block in
// one piece (scratch sized from `Prepare::max_block`) and re-seat once per
// block where their step would have put them, and the digests are unchanged
// (tutti-sampler's `tests/block_length_invariance.rs` pins that a voice's
// frames do not depend on its blocks).
pub const GRAPH_MAX_BLOCK: Samples = Samples(1024);

/// How a [`RenderGraph`] is assembled beyond its pair: the pool the render
/// spreads its blocks over, and what the graph's output channels are.
///
/// `Setup::default()` is the render's own thread and the engine's order for
/// the width, which is what a graph built in a test or a simple host wants.
#[derive(Debug, Clone, Default)]
pub struct Setup {
    pool: Option<Arc<dyn Pool>>,
    root: Option<ChannelTopology>,
}

impl Setup {
    /// A render on its own thread, the output channels in the engine's order
    /// for the width.
    pub fn new() -> Self {
        Self::default()
    }

    /// Render on `pool` (the parallel executor, doc 013), or with `None` on
    /// the render's own thread. The render is bit for bit the same either
    /// way; a pool only spreads each block's independent nodes across its
    /// threads. Give an export its own pool (a `tutti_graph::WorkerPool`),
    /// not the live engine's: a job finds a pool busy with another caller's
    /// and runs on its caller alone.
    #[must_use]
    pub fn with_pool(mut self, pool: Option<Arc<dyn Pool>>) -> Self {
        self.pool = pool;
        self
    }

    /// States what the graph's output channels are, for the fold to the file:
    /// [`ChannelTopology::unassigned`] for independent buses, a named topology
    /// for speaker feeds. `None` (the default) is the engine's order for the
    /// width, as for a live graph that declared nothing.
    ///
    /// The render refuses a declaration that is not as wide as the graph's
    /// outputs, or that the file cannot take
    /// ([`ConfigError::Placement`]). Give it for a fork of a live graph that
    /// declared its root, so the file is placed as the device was.
    #[must_use]
    pub fn with_root(mut self, root: Option<ChannelTopology>) -> Self {
        self.root = root;
        self
    }
}

/// The graph an export renders: a `tutti_graph` editor/executor pair.
///
/// The pair is already installed (the executor running its plan) and
/// prepared at the render's sample rate: the render refuses any other
/// rate rather than re-rating it, since a unit's preparation is the control
/// side's job. A `RenderGraph` is made one way, [`RenderGraph::assemble`], from
/// a pair that was built together:
///
/// - build it at [`RenderGraph::prepare`] (a `GraphBuilder` in a test or a
///   simple host) and assemble what `build` returns;
/// - or, for a fork of a live graph, `tutti-session` forks, edits and
///   assembles it (the fork machinery is the session's, not this crate's).
///
/// # The pair
///
/// The editor must feed the executor: it is what the render drains of the
/// units and commits the executor retires, and an editor paired with some
/// other executor would never see them. [`assemble`](Self::assemble) checks
/// it, and nothing afterwards can break it: a `RenderGraph` hands out neither
/// half (an edit is made on the pair before it is assembled), so the
/// check is made once, at the one constructor.
///
/// The executor renders in blocks of its prepared `MaxBlock`, each handed the
/// transport the render's clock reports ([`RenderClock::graph_block`]), and
/// the clock is advanced after each block ([`RenderClock::render_graph`]).
/// A clip reader reads the transport at any frame of its block from its
/// `Env`.
///
/// [`RenderClock::graph_block`]: tutti_transport::RenderClock::graph_block
/// [`RenderClock::render_graph`]: tutti_transport::RenderClock::render_graph
///
/// # Latency and tail
///
/// [`reported_latency`](Self::reported_latency) and
/// [`reported_tail`](Self::reported_tail) answer from what the graph already
/// holds — the compiled plan's worst-case output latency and the tail fold
/// over its spec. The figures go into [`RenderConfig`](crate::RenderConfig),
/// where the leading trim and the tail extension are plain arithmetic.
///
/// # Example
///
/// ```
/// use tutti_types::{Hz, SampleRate, Seconds};
/// use tutti_export::{render_one, Control, ExportConfig, FrozenClock, Output, Produced, RenderConfig, RenderGraph, Setup};
/// use tutti_graph::GraphBuilder;
/// use tutti_nodes::testing::Osc;
/// use tutti_types::ChannelLayout;
///
/// let rate = SampleRate(48_000.0);
/// let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
/// let tone = g.add(Osc::sine(Hz(440.0)));
/// g.pipe_output(tone);
/// let (editor, executor) = g.build(RenderGraph::prepare(rate)).expect("builds");
/// let mut graph = RenderGraph::assemble(editor, executor, Setup::new()).expect("built together, so paired");
///
/// let config = ExportConfig {
///     render: RenderConfig { sample_rate: rate, duration_seconds: Seconds(0.1), ..Default::default() },
///     ..Default::default()
/// };
/// let out = render_one(
///     &mut graph,
///     &config.job().expect("a render"),
///     &FrozenClock,
///     Output::master(config.buffer_target()),
///     &mut Control::none(),
/// )
/// .expect("renders")
/// .finished()
/// .and_then(Produced::buffers)
/// .expect("nothing cancels it");
/// assert_eq!(out.frames().get(), 4_800);
/// ```
#[derive(Debug)]
pub struct RenderGraph {
    /// The control side. Drained after every block, so what the executor
    /// retires is freed on the render thread.
    editor: Editor,
    /// The executor the render drives. Never handed out: the render is the
    /// only thing that runs it.
    executor: Executor,
    /// What the graph's output channels are, when the live graph declared it
    /// ([`Setup::with_root`]); `None` is the engine's order for the width.
    root: Option<ChannelTopology>,
    /// The executor's `(dropped, recovered)` event counts where the last
    /// render ended, or where the pair was wrapped: the next render counts its
    /// losses from here, so a drop between the two (installing a commit) is
    /// the next render's, and a drop in an earlier render is not.
    settled: (u64, u64),
}

impl RenderGraph {
    /// Assemble an installed editor/executor pair, with its [`Setup`], for a
    /// render. The one constructor.
    ///
    /// A pool the executor already runs on stays unless `setup` names one.
    ///
    /// The pair is already installed: edit and commit the editor before this
    /// (`Editor::commit`, then `Executor::apply_pending` and `Editor::collect`),
    /// because nothing here hands either half out again.
    ///
    /// # Errors
    ///
    /// [`ConfigError::UnpairedGraph`] if `editor` does not feed `executor`
    /// (`Editor::is_paired_with`): pass the pair built or forked together.
    /// [`ConfigError::InvalidRate`] if the pair was prepared at a rate that is
    /// not finite and positive. That the pair's rate is the render's is
    /// checked at render, where the [`Job`](crate::Job) names it
    /// ([`ConfigError::RateMismatch`]).
    pub fn assemble(editor: Editor, mut executor: Executor, setup: Setup) -> Result<Self> {
        if !editor.is_paired_with(&executor) {
            return Err(ConfigError::UnpairedGraph.into());
        }
        let prepared = executor.prepare().sample_rate().get();
        if !(prepared.is_finite() && prepared > 0.0) {
            return Err(ConfigError::InvalidRate { prepared }.into());
        }
        if setup.pool.is_some() {
            executor.set_pool(setup.pool);
        }
        let settled = (
            executor.dropped_event_count(),
            executor.recovered_event_count(),
        );
        Ok(Self {
            editor,
            executor,
            root: setup.root,
            settled,
        })
    }

    /// Where the next render counts its event losses from.
    pub(crate) fn settled(&self) -> (u64, u64) {
        self.settled
    }

    /// The executor's counts at the end of a render: the next one starts here.
    pub(crate) fn settle(&mut self, counts: (u64, u64)) {
        self.settled = counts;
    }

    /// The declared output topology ([`Setup::with_root`]).
    pub(crate) fn root_topology(&self) -> Option<&ChannelTopology> {
        self.root.as_ref()
    }

    /// Both halves, for the render. Crate-private: outside the crate the
    /// executor is never handed out.
    pub(crate) fn parts_mut(&mut self) -> (&mut Editor, &mut Executor) {
        (&mut self.editor, &mut self.executor)
    }

    /// What a graph is prepared at to render at `sample_rate`: that
    /// rate, and [`GRAPH_MAX_BLOCK`].
    pub fn prepare(sample_rate: SampleRate) -> Prepare {
        Prepare::new(sample_rate, GRAPH_MAX_BLOCK)
    }

    /// The look-ahead latency the graph reports, as a frame count — the
    /// figure to put in [`RenderConfig::latency`](crate::RenderConfig::latency)
    /// to trim it.
    ///
    /// Read from the compiled plan: the worst-case latency across the
    /// graph's outputs (`Plan::total_latency`), the figure its PDC aligned
    /// every output to, at the rate the graph was prepared at. An executor
    /// with no plan installed has nothing to delay, and reports zero.
    ///
    /// It is a method rather than a `LatencyTrim::Reported` mode on the
    /// config because asking a graph is an *action*, and folding it into a
    /// value would drag a graph into arithmetic that is otherwise pure:
    ///
    /// ```
    /// # use tutti_types::{Hz, SampleRate};
    /// # use tutti_export::{ExportConfig, RenderConfig, RenderGraph, Setup};
    /// # use tutti_graph::GraphBuilder;
    /// # use tutti_nodes::testing::Osc;
    /// # use tutti_types::ChannelLayout;
    /// # let rate = SampleRate(48_000.0);
    /// # let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    /// # let tone = g.add(Osc::sine(Hz(440.0)));
    /// # g.pipe_output(tone);
    /// # let (editor, executor) = g.build(RenderGraph::prepare(rate)).unwrap();
    /// # let graph = RenderGraph::assemble(editor, executor, Setup::new()).unwrap();
    /// let latency = graph.reported_latency();
    /// let config = ExportConfig {
    ///     render: RenderConfig { sample_rate: rate, latency, ..Default::default() },
    ///     ..Default::default()
    /// };
    /// # let _ = config;
    /// ```
    ///
    /// Whole frames: the plan's latency is a frame count already.
    pub fn reported_latency(&self) -> Samples {
        self.executor
            .plan()
            .map_or(Samples::ZERO, |plan| plan.total_latency().samples())
    }

    /// The tail the graph reports — how long it keeps ringing after its input
    /// stops — with its caveats: the fold over the graph's topology
    /// (`tutti_graph::graph_tail`), whose per-node tails the editor probed
    /// from each prepared unit.
    ///
    /// For a caller that wants [`RenderConfig::tail`](crate::RenderConfig::tail)
    /// to be whatever the graph says: a reverb, a convolver, a hosted plugin
    /// that declared a decay. It returns the figure and its caveats
    /// rather than a frame count, because for two graphs there is no count:
    /// one that never decays, and one whose nodes were never taught to
    /// answer. Resolving either into a number is a decision, so it happens at
    /// the call site:
    ///
    /// ```
    /// # use tutti_types::{Hz, SampleRate, Seconds};
    /// # use tutti_export::{ExportConfig, RenderConfig, RenderGraph, Setup};
    /// # use tutti_graph::GraphBuilder;
    /// # use tutti_nodes::testing::Osc;
    /// # use tutti_types::ChannelLayout;
    /// # let rate = SampleRate(48_000.0);
    /// # let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    /// # let tone = g.add(Osc::sine(Hz(440.0)));
    /// # g.pipe_output(tone);
    /// # let (editor, executor) = g.build(RenderGraph::prepare(rate)).unwrap();
    /// # let graph = RenderGraph::assemble(editor, executor, Setup::new()).unwrap();
    /// let reported = graph.reported_tail();
    /// let tail = reported.samples().unwrap_or_else(|| {
    ///     // This bounce stops four seconds into an unbounded tail.
    ///     Seconds(4.0).to_samples(rate)
    /// });
    /// let config = ExportConfig {
    ///     render: RenderConfig { sample_rate: rate, tail, ..Default::default() },
    ///     ..Default::default()
    /// };
    /// # let _ = config;
    /// ```
    pub fn reported_tail(&self) -> GraphTail {
        tutti_graph::graph_tail(&self.editor.spec().topology)
    }
}
