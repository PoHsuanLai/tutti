//! What an export renders: the native graph.
//!
//! Doc 013 Phase 3. PR 7 put [`RenderGraph`] beside fundsp's `Net` as a second
//! backend; PR 14 removed the `Net` one, so an export renders a `tutti_graph`
//! editor/executor pair and nothing else. It becomes a frame source
//! (`render::driver`), and the gate, resample, dither and encoders downstream
//! see only frames.

use tutti_core::SampleRate;
use tutti_graph::{Editor, Executor, ForkError, ForkMode, ForkTarget, Prepare};
use tutti_types::{GraphTail, Samples};

use crate::{Error, Result};

/// The `MaxBlock` a native graph is prepared at for an export
/// ([`RenderGraph::prepare`], [`RenderGraph::fork`]).
///
/// 1024 frames: long enough that the executor's per-block walk is paid rarely
/// on a long bounce, short enough that a block's planes stay in cache. A
/// multiple of 64 on purpose: a `Legacy` unit runs in 64-frame chunks from
/// each block's start, so at a multiple of 64 its chunks fall on the frames
/// `Net`'s 64-frame blocks did, and a unit whose output depends on how its
/// calls are cut (the VBAP panner ramps its gains across each call) renders
/// what it rendered under `Net` (doc 013, "Two things carry over from
/// `Legacy` chunking"; pinned by `tests/graph_source.rs`).
pub const GRAPH_MAX_BLOCK: Samples = Samples(1024);

/// The graph an export renders: a native `tutti_graph` editor/executor pair.
///
/// The pair is already installed (the executor running its plan) and
/// prepared **at the render's sample rate** — the render refuses any other
/// rate rather than re-rating it, since a unit's preparation is the control
/// side's job. Get one of two ways:
///
/// - [`RenderGraph::fork`] a live graph: the export path. It forks at the
///   render's rate and [`GRAPH_MAX_BLOCK`], and a node that cannot be forked
///   is [`Error::NotForkable`] naming it.
/// - Build one at [`RenderGraph::prepare`] (a `GraphBuilder` in a test or a
///   simple host) and wrap the pair: `RenderGraph { editor, executor }`.
///
/// The fields are public so a caller can edit the graph before it renders
/// (bevy-tutti's export hook inserts nodes through `editor`). The render
/// checks that `editor` feeds `executor` and refuses a pair that does not.
///
/// The executor renders in blocks of its prepared `MaxBlock`, each handed the
/// transport the render's clock reports ([`RenderClock::graph_block`]), and
/// the clock is advanced after each block. A graph holding a `Legacy` unit (a
/// sampler voice, which polls the clock per 64-frame call) is rendered
/// chunk-major, 64 frames at a time across every node
/// ([`RenderClock::render_graph`]), so its clip readers read the clock where
/// each chunk starts.
///
/// [`RenderClock::graph_block`]: tutti_core::transport::RenderClock::graph_block
/// [`RenderClock::render_graph`]: tutti_core::transport::RenderClock::render_graph
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
/// use tutti_core::{Hz, SampleRate};
/// use tutti_export::{render_to_buffers, ExportConfig, FrozenClock, RenderConfig, RenderGraph};
/// use tutti_graph::GraphBuilder;
/// use tutti_nodes::testing::Osc;
/// use tutti_types::ChannelLayout;
///
/// let rate = SampleRate(48_000.0);
/// let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
/// let tone = g.add_unit(Box::new(Osc::sine(Hz(440.0))));
/// g.pipe_output(tone);
/// let (editor, executor) = g.build(RenderGraph::prepare(rate)).expect("builds");
///
/// let config = ExportConfig {
///     render: RenderConfig { sample_rate: rate, duration_seconds: 0.1, ..Default::default() },
///     ..Default::default()
/// };
/// let out = render_to_buffers(RenderGraph { editor, executor }, &config, &FrozenClock)
///     .expect("renders");
/// assert_eq!(out.frames().get(), 4_800);
/// ```
pub struct RenderGraph {
    /// The control side. Drained after every block, so what the executor
    /// retires is freed on the render thread.
    pub editor: Editor,
    /// The executor the render drives.
    pub executor: Executor,
}

impl RenderGraph {
    /// What a native graph is prepared at to render at `sample_rate`: that
    /// rate, and [`GRAPH_MAX_BLOCK`].
    pub fn prepare(sample_rate: SampleRate) -> Prepare {
        Prepare::new(sample_rate, GRAPH_MAX_BLOCK)
    }

    /// Fork `target` out of the live graph `live` for an export at
    /// `sample_rate`. The live graph is not touched (`Editor::fork` reads the
    /// spec and the nodes' fork sources, and sends nothing).
    ///
    /// `mode` is normally `ForkMode::Offline(&transport)`, with `transport`
    /// the render's `OfflineTransport` (tutti-core) — the value itself; see
    /// `ForkMode::Offline` for what a wrong type does (nothing, silently).
    ///
    /// # Errors
    ///
    /// [`Error::NotForkable`] naming the first node that cannot be forked (a
    /// mic monitor, an in-process VST2 plugin, a plugin inserted as a boxed
    /// `AudioUnit` rather than a `PluginClient`), checked before anything is
    /// forked; any other fork failure is [`Error::Fork`] (a plugin fork whose
    /// fresh instance did not load or refused the state is
    /// `ForkError::Source`). A fork that fails *during* the render is
    /// [`Error::ForkFailed`] from the render call.
    pub fn fork(
        live: &Editor,
        target: ForkTarget,
        mode: ForkMode<'_>,
        sample_rate: SampleRate,
    ) -> Result<Self> {
        let (editor, executor) = live
            .fork(target, mode, Self::prepare(sample_rate))
            .map_err(|e| match e {
                ForkError::NotForkable { key } => Error::NotForkable { key },
                other => Error::Fork(other),
            })?;
        Ok(Self { editor, executor })
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
    /// # use tutti_core::{Hz, SampleRate};
    /// # use tutti_export::{ExportConfig, RenderConfig, RenderGraph};
    /// # use tutti_graph::GraphBuilder;
    /// # use tutti_nodes::testing::Osc;
    /// # use tutti_types::ChannelLayout;
    /// # let rate = SampleRate(48_000.0);
    /// # let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    /// # let tone = g.add_unit(Box::new(Osc::sine(Hz(440.0))));
    /// # g.pipe_output(tone);
    /// # let (editor, executor) = g.build(RenderGraph::prepare(rate)).unwrap();
    /// # let graph = RenderGraph { editor, executor };
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
    /// (`tutti_types::graph_tail`), whose per-node tails the editor probed
    /// from each prepared unit.
    ///
    /// For a caller that wants [`RenderConfig::tail`](crate::RenderConfig::tail)
    /// to be whatever the graph says: a reverb, a convolver, a hosted plugin
    /// that declared a decay. It returns the figure **and its caveats**
    /// rather than a frame count, because for two graphs there is no count:
    /// one that never decays, and one whose nodes were never taught to
    /// answer. Resolving either into a number is a decision, so it happens at
    /// the call site:
    ///
    /// ```
    /// # use tutti_core::{Hz, SampleRate, Seconds};
    /// # use tutti_export::{ExportConfig, RenderConfig, RenderGraph};
    /// # use tutti_graph::GraphBuilder;
    /// # use tutti_nodes::testing::Osc;
    /// # use tutti_types::ChannelLayout;
    /// # let rate = SampleRate(48_000.0);
    /// # let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    /// # let tone = g.add_unit(Box::new(Osc::sine(Hz(440.0))));
    /// # g.pipe_output(tone);
    /// # let (editor, executor) = g.build(RenderGraph::prepare(rate)).unwrap();
    /// # let graph = RenderGraph { editor, executor };
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
        tutti_types::graph_tail(&self.editor.spec().topology)
    }
}
