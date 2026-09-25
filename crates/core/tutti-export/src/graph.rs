//! What an export renders: fundsp's `Net`, or the native graph.
//!
//! Doc 013 Phase 3 PR 7. The two backends meet at [`RenderGraph`] and part
//! nowhere after it: both become a frame source (`render::driver`), and the
//! gate, resample, dither and encoders downstream do not know which one they
//! are pulling. PR 14 deletes the `Net` arm.

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
/// `Net`'s 64-frame blocks do, and a block-oriented unit (a convolver's FFT
/// partitions) renders the same samples through either backend (doc 013,
/// "Two things carry over from `Legacy` chunking").
pub const GRAPH_MAX_BLOCK: Samples = Samples(1024);

/// The graph an export renders, in either backend.
///
/// Every render entry point ([`render_to_file`](crate::render_to_file),
/// [`render_to_buffers`](crate::render_to_buffers),
/// [`render_normalized_to_file`](crate::render_normalized_to_file)) takes
/// `impl Into<RenderGraph>`, and a `Net` converts on its own, so existing
/// callers pass their `Net` unchanged.
///
/// # The `Graph` backend
///
/// A native `tutti_graph` editor/executor pair, already installed (the
/// executor running its plan), prepared **at the render's sample rate** — the
/// render refuses any other rate rather than re-rating it, since a unit's
/// preparation is the control side's job. Get one of two ways:
///
/// - [`RenderGraph::fork`] a live graph: the export path. It forks at the
///   render's rate and [`GRAPH_MAX_BLOCK`], and a node that cannot be forked
///   is [`Error::NotForkable`] naming it.
/// - Build one at [`RenderGraph::prepare`] (a `GraphBuilder` in a test or a
///   simple host) and wrap the pair in the variant.
///
/// The executor renders in blocks of its prepared `MaxBlock`, each handed the
/// transport the render's clock reports ([`RenderClock::graph_block`]), and
/// the clock is advanced after each block, as for a `Net`.
///
/// [`RenderClock::graph_block`]: tutti_core::transport::RenderClock::graph_block
///
/// # Latency and tail
///
/// [`reported_latency`](Self::reported_latency) and
/// [`reported_tail`](Self::reported_tail) answer for either backend: a `Net`
/// is asked (`AudioUnit::latency`, the tail fold over its nodes), and the
/// native graph answers from what it already holds — the compiled plan's
/// worst-case output latency and the tail fold over its spec. The figures go
/// into [`RenderConfig`](crate::RenderConfig) exactly as before, so the
/// leading trim and the tail extension are one code path for both.
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
/// let out = render_to_buffers(RenderGraph::Graph { editor, executor }, &config, &FrozenClock)
///     .expect("renders");
/// assert_eq!(out.frames().get(), 4_800);
/// ```
pub enum RenderGraph {
    /// fundsp's `Net`, re-rated to the render's rate and pulled in 64-frame
    /// blocks.
    Net(tutti_core::dsp::Net),
    /// A native graph: an installed editor/executor pair, built together and
    /// prepared at the render's rate.
    Graph {
        /// The control side. Drained after every block, so what the executor
        /// retires is freed on the render thread.
        editor: Editor,
        /// The executor the render drives.
        executor: Executor,
    },
}

impl From<tutti_core::dsp::Net> for RenderGraph {
    fn from(net: tutti_core::dsp::Net) -> Self {
        Self::Net(net)
    }
}

impl RenderGraph {
    /// What a native graph is prepared at to render at `sample_rate`: that
    /// rate, and [`GRAPH_MAX_BLOCK`].
    pub fn prepare(sample_rate: SampleRate) -> Prepare {
        Prepare::new(sample_rate, GRAPH_MAX_BLOCK)
    }

    /// Fork `target` out of the live graph `live` for an export at
    /// `sample_rate` — the native counterpart of cloning a `Net` for a
    /// render. The live graph is not touched (`Editor::fork` reads the spec
    /// and the nodes' fork sources, and sends nothing).
    ///
    /// `mode` is normally `ForkMode::Offline(&transport)`, with `transport`
    /// the render's `OfflineTransport` (tutti-core) — the value itself; see
    /// `ForkMode::Offline` for what a wrong type does (nothing, silently).
    ///
    /// # Errors
    ///
    /// [`Error::NotForkable`] naming the first node that cannot be forked (a
    /// plugin, a mic monitor), checked before anything is forked; any other
    /// fork failure is [`Error::Fork`].
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
        Ok(Self::Graph { editor, executor })
    }

    /// The look-ahead latency the graph reports, as a frame count — the
    /// figure to put in [`RenderConfig::latency`](crate::RenderConfig::latency)
    /// to trim it.
    ///
    /// A `Net` is asked, as [`reported_latency`](crate::reported_latency)
    /// does. The native graph answers from its compiled plan: the worst-case
    /// latency across its outputs (`Plan::total_latency`), the figure its PDC
    /// aligned every output to. An executor with no plan installed has
    /// nothing to delay, and reports zero.
    pub fn reported_latency(&mut self) -> Samples {
        match self {
            Self::Net(net) => crate::reported_latency(net),
            Self::Graph { executor, .. } => executor
                .plan()
                .map_or(Samples::ZERO, |plan| plan.total_latency().samples()),
        }
    }

    /// The tail the graph reports — how long it keeps ringing after its input
    /// stops — with its caveats; see [`reported_tail`](crate::reported_tail)
    /// for resolving it into [`RenderConfig::tail`](crate::RenderConfig::tail).
    ///
    /// The same fold for both backends (`tutti_types::graph_tail`), over a
    /// `Net`'s nodes or over the native graph's topology, whose per-node
    /// tails the editor probed from each prepared unit.
    pub fn reported_tail(&self) -> GraphTail {
        match self {
            Self::Net(net) => crate::reported_tail(net),
            Self::Graph { editor, .. } => tutti_types::graph_tail(&editor.spec().topology),
        }
    }
}
