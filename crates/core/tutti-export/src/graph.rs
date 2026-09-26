//! What an export renders: the native graph.
//!
//! Doc 013 Phase 3. PR 7 put [`RenderGraph`] beside fundsp's `Net` as a second
//! backend; PR 14 removed the `Net` one, so an export renders a `tutti_graph`
//! editor/executor pair and nothing else. It becomes a frame source
//! (`render::driver`), and the gate, resample, dither and encoders downstream
//! see only frames.

use tutti_core::SampleRate;
use tutti_graph::{CommitError, Editor, Executor, ForkError, ForkMode, ForkTarget, Prepare};
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
///   simple host) and wrap the pair with [`RenderGraph::new`].
///
/// # The pair
///
/// The editor must feed the executor: it is what the render drains of the
/// units and commits the executor retires, and an editor paired with some
/// other executor would never see them. The fields are private and both
/// constructors check it, so a `RenderGraph` holds a matched pair from the
/// start. A caller edits the graph before it renders through
/// [`editor_mut`](Self::editor_mut) and sends the edit with
/// [`commit`](Self::commit) (bevy-tutti's export hook does both); the
/// executor is never handed out.
///
/// The render checks the pairing again, as a real error rather than a
/// `debug_assert`: `editor_mut` hands out `&mut Editor`, which
/// `std::mem::replace` can swap for an editor of another graph, and a check
/// that costs one comparison per render is cheaper than a render whose
/// retirees are never collected.
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
/// let graph = RenderGraph::new(editor, executor).expect("built together, so paired");
///
/// let config = ExportConfig {
///     render: RenderConfig { sample_rate: rate, duration_seconds: 0.1, ..Default::default() },
///     ..Default::default()
/// };
/// let out = render_to_buffers(graph, &config, &FrozenClock).expect("renders");
/// assert_eq!(out.frames().get(), 4_800);
/// ```
pub struct RenderGraph {
    /// The control side. Drained after every block, so what the executor
    /// retires is freed on the render thread.
    editor: Editor,
    /// The executor the render drives. Never handed out: the render is the
    /// only thing that runs it.
    executor: Executor,
}

impl RenderGraph {
    /// Wrap an installed editor/executor pair for a render.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidConfig`] if `editor` does not feed `executor`
    /// (`Editor::is_paired_with`): pass the pair built or forked together.
    /// The rate is checked at render, against the render's config.
    pub fn new(editor: Editor, executor: Executor) -> Result<Self> {
        let graph = Self { editor, executor };
        graph.check_paired()?;
        Ok(graph)
    }

    /// Refuse a pair whose editor does not feed its executor. [`new`](Self::new)
    /// and the render both ask (see "The pair" above for why twice).
    pub(crate) fn check_paired(&self) -> Result<()> {
        if self.editor.is_paired_with(&self.executor) {
            Ok(())
        } else {
            Err(Error::InvalidConfig(
                "the graph's editor does not feed its executor; pass the pair built together"
                    .into(),
            ))
        }
    }

    /// The graph's editor, to read its spec.
    pub fn editor(&self) -> &Editor {
        &self.editor
    }

    /// The graph's editor, to edit the graph before it renders (insert a
    /// node, rewire through `spec_mut`). Send the edit with
    /// [`commit`](Self::commit); the render does not commit for you.
    pub fn editor_mut(&mut self) -> &mut Editor {
        &mut self.editor
    }

    /// Send what [`editor_mut`](Self::editor_mut) edited, and install it on
    /// the executor now, so the render's first block already runs it.
    ///
    /// # Errors
    ///
    /// The editor's [`CommitError`] if the edit does not compile (a cycle, a
    /// port out of range) or the editor is poisoned; nothing is installed.
    pub fn commit(&mut self) -> std::result::Result<(), CommitError> {
        self.editor.commit()?;
        self.executor.apply_pending();
        self.editor.collect();
        Ok(())
    }

    /// Both halves, for the render. Crate-private: outside the crate the
    /// executor is never handed out.
    pub(crate) fn parts_mut(&mut self) -> (&mut Editor, &mut Executor) {
        (&mut self.editor, &mut self.executor)
    }

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
    /// the render's `OfflineTransport`: typed, so nothing else compiles.
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
        // A fork returns its own installed pair; checked all the same, so no
        // constructor skips the pairing.
        Self::new(editor, executor)
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
    /// # let graph = RenderGraph::new(editor, executor).unwrap();
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
    /// # let graph = RenderGraph::new(editor, executor).unwrap();
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
