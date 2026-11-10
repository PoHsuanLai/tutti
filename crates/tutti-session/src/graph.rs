//! [`Graph`]: a `tutti-graph` [`Editor`], the nodes inserted through it, the
//! event feeds into them, the crossfades waiting on a re-prepare, and — until
//! an engine or a test takes it — the [`Executor`] it sends to.
//!
//! What each method does to the editor:
//!
//! | `Graph` | the editor |
//! |---|---|
//! | `insert` | `Editor::insert` of the node's own [`IntoNode`] at a [`NodeKey`] minted by `NodeKey::fresh`, its controls handed back |
//! | `set_source`, `set_output_source`, `widen_outputs` | written into `editor.spec_mut()` |
//! | `set_param` | the node's [`ParamSet`], when it has one |
//! | `replace`, `crossfade` | `Editor::replace_or_swap` with a [`Fade`]: a plain swap when there is nothing to fade from |
//! | `node_latency`, `node_tail`, `node_inputs`, `node_outputs` | the editor's [`Shapes`](tutti_graph::Shapes) |
//! | `latency_plan` | `tutti_graph::latency_plan` over the whole spec: audio edges plus event and param-modulation sources |
//! | `set_feed`, `move_feeds` | the union of every feeder's sources, written as a sink's event edges beside the host's own |
//! | `refresh_node_latency` | `Editor::set_latency` |
//! | `insert_plugin`, `replace_plugin` | the bound `PluginClient` itself, a node with its own fork source |
//!
//! The commit is the [`Session`](crate::Session)'s: it publishes the plan's
//! compensation with it.
//!
//! # Where the audio side lives
//!
//! `Editor::new` builds the pair. The executor stays here ("local") until
//! [`take_audio_side`](Graph::take_audio_side) or the session builder hands it
//! to an engine. While it is local, a commit is applied at once on this
//! thread and its box collected, so a detached graph never meets
//! back-pressure, and [`Session::render_frame`](crate::Session::render_frame)
//! runs it.
//!
//! # `set_param` lands on the next block, never at once
//!
//! There is no control-side copy of a node for a setting to land on at once.
//! A write goes into the node's own `Param` cell through its [`ParamSet`], and
//! the node reads it at the start of the executor's next block: one block
//! later, on every graph.

use std::any::Any;
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use tutti_graph::Compensation;
use tutti_graph::CrossfadeCurve;
use tutti_graph::{
    CommitError, Editor, EventEdge, EventIn, EventOut, EventTapReader, Executor, Fade,
    GraphInvalid, InletNode, IntoNode, MeterReader, MissingSource, ParamIn, ParamMod, ParamRange,
    ParamSet, Prepare, TapReader, TapWriter, Transport,
};
use tutti_graph::{Edge, InPort, NodeKey, OutPort, Source};
use tutti_graph::{EventTapId, MeterId, MeterSpec, TapAlign, TapId};
#[cfg(feature = "plugin")]
use tutti_types::Latency;
use tutti_types::{ChannelLayout, SampleRate, Seconds, UnitAddr};
use tutti_types::{Samples, Tail};

use crate::{InletError, SessionNode};

/// The largest block a device's graph is prepared for.
///
/// A device block longer than this is rendered by `tutti_engine::Engine` as
/// consecutive graph blocks of at most this many frames, so it bounds the
/// arena, not the device. 1024 frames covers every buffer size a DAW offers
/// by default.
pub const LIVE_MAX_BLOCK: Samples = Samples(1024);

/// The spec `kind` of a hosted plugin.
#[cfg(feature = "plugin")]
const PLUGIN_KIND: &str = "tutti-session:plugin";

/// Where one input port — a node's, or a global output channel — reads from.
///
/// The graph's answer to "what feeds this port", naming the node by its
/// [`NodeKey`]. Read back with [`Graph::source`] and
/// [`Graph::output_source`]; written with [`Graph::set_source`] and
/// [`Graph::set_output_source`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum GraphSource {
    /// Output `port` of `node`.
    Node(NodeKey, usize),
    /// Global graph input `port` — a hardware or host input channel.
    Input(usize),
    /// Silence: what a port nothing has written holds.
    #[default]
    Silence,
}

impl GraphSource {
    /// Output `port` of `node`.
    pub const fn node(node: NodeKey, port: usize) -> Self {
        Self::Node(node, port)
    }
}

/// One event output: `node`'s event output `port`. From an [`NodeKey`]
/// alone, its output 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EventSource {
    /// The node.
    pub node: NodeKey,
    /// Its event output.
    pub port: u16,
}

impl EventSource {
    /// Names `node`'s event output `port`.
    #[must_use]
    pub const fn new(node: NodeKey, port: u16) -> Self {
        Self { node, port }
    }

    /// A total order for a set of sources: by node, then port.
    fn key(&self) -> (u64, u16) {
        (self.node.0, self.port)
    }
}

impl From<NodeKey> for EventSource {
    fn from(node: NodeKey) -> Self {
        Self::new(node, 0)
    }
}

/// Why [`Graph::replace`] did not take a node — or, as
/// `ReplaceRefused<Box<PluginClient>>`, `Graph::replace_plugin` (feature
/// `plugin`) a plugin.
pub enum ReplaceRefused<U> {
    /// Not now: the graph is re-preparing (a sample-rate or block-size change
    /// between its two commits). The node is handed back, untouched; retry
    /// once the re-prepare has resumed — [`Graph::crossfade`] keeps it
    /// pending and `update()` does.
    Busy(U),
    /// Never: the graph is poisoned (a re-prepare failed with its units out),
    /// or the node is not in it. The incoming node was dropped.
    Failed(CrossfadeRefused),
}

impl<U> std::fmt::Debug for ReplaceRefused<U> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy(_) => f.write_str("Busy(..)"),
            Self::Failed(refused) => f.debug_tuple("Failed").field(refused).finish(),
        }
    }
}

/// Who feeds a sink's event input 0 through [`Graph::set_feed`]. Each feeder
/// owns its own entry, so feeders never overwrite each other.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Feeder {
    /// A clip the session's sequencer plays.
    Sequencer,
    /// A live input the session made.
    LiveInput,
    /// The session's MIDI route rules.
    Routes,
    /// The timed queue of an instrument's handle
    /// (`Session::add_instrument`, feature `midi`).
    Instrument,
    /// The clock the hardware out carries.
    Clock,
    /// A host's declarations (bevy-tutti's `EventSources`).
    Declared,
}

/// A crossfade waiting for a re-prepare, named so its landing can be told
/// from another's ([`SessionEvent::CrossfadeLanded`](crate::SessionEvent)).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CrossfadeId(u64);

/// What [`Graph::crossfade`] came to, when the graph did not refuse it.
#[derive(Debug)]
pub enum Crossfade<C> {
    /// Swapped in: the incoming node's controls, its params addressed.
    Landed(C),
    /// The graph is re-preparing: the node waits, and the update that finds
    /// the graph resumed swaps it in and reports
    /// [`SessionEvent::CrossfadeLanded`](crate::SessionEvent) with this id
    /// (its controls boxed), or `CrossfadeFailed`.
    Waiting(CrossfadeId),
}

/// Why a swap ([`Graph::replace`], [`Graph::crossfade`]) took nothing. The
/// incoming node was dropped.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CrossfadeRefused {
    /// The graph is poisoned (a re-prepare failed with its units out); the
    /// cause is the editor's.
    #[error("the graph is poisoned ({0}); build a new one")]
    Poisoned(String),
    /// The node is not in the graph.
    #[error("{0:?} is not in the graph")]
    NoSuchNode(NodeKey),
    /// The editor refused the swap.
    #[error("the graph refused the swap: {0}")]
    Editor(tutti_graph::CommitError),
}

/// One node's handle, and its params by address when it has any.
struct Entry {
    node: NodeKey,
    /// What [`set_param`](Graph::set_param) writes through, and what a fork
    /// of the node starts from.
    params: Option<ParamSet>,
    /// What frees the values its published cells retired
    /// ([`SessionNode::reclaimer`]), run by [`Graph::reclaim`].
    reclaimer: Option<crate::Reclaimer>,
}

/// The executor, while this side still holds it, and what a local render
/// needs beside it.
struct Local {
    exec: Executor,
    /// Planar scratch, one `Vec` per global output.
    scratch: Vec<Vec<f32>>,
}

/// A node waiting to be swapped in, its type erased.
trait ErasedSwap: Send {
    /// Swap it in under `node`; on success, its controls, boxed.
    fn swap(
        self: Box<Self>,
        graph: &mut Graph,
        node: NodeKey,
        fade: Seconds,
        curve: CrossfadeCurve,
    ) -> Result<Box<dyn Any + Send>, ReplaceRefused<Box<dyn ErasedSwap>>>;
}

struct Swap<N>(N);

impl<N> ErasedSwap for Swap<N>
where
    N: SessionNode,
    N::Controls: Send + 'static,
{
    fn swap(
        self: Box<Self>,
        graph: &mut Graph,
        node: NodeKey,
        fade: Seconds,
        curve: CrossfadeCurve,
    ) -> Result<Box<dyn Any + Send>, ReplaceRefused<Box<dyn ErasedSwap>>> {
        match graph.replace(node, self.0, fade, curve) {
            Ok(controls) => {
                graph.address::<N>(node, &controls);
                Ok(Box::new(controls))
            }
            Err(ReplaceRefused::Busy(n)) => Err(ReplaceRefused::Busy(Box::new(Swap(n)))),
            Err(ReplaceRefused::Failed(refused)) => Err(ReplaceRefused::Failed(refused)),
        }
    }
}

#[cfg(feature = "plugin")]
struct PluginSwap(Box<tutti_plugin::handles::PluginClient>);

#[cfg(feature = "plugin")]
impl ErasedSwap for PluginSwap {
    fn swap(
        self: Box<Self>,
        graph: &mut Graph,
        node: NodeKey,
        fade: Seconds,
        curve: CrossfadeCurve,
    ) -> Result<Box<dyn Any + Send>, ReplaceRefused<Box<dyn ErasedSwap>>> {
        match graph.replace_plugin(node, self.0, fade, curve) {
            Ok(()) => Ok(Box::new(())),
            Err(ReplaceRefused::Busy(c)) => Err(ReplaceRefused::Busy(Box::new(PluginSwap(c)))),
            Err(ReplaceRefused::Failed(refused)) => Err(ReplaceRefused::Failed(refused)),
        }
    }
}

/// A crossfade parked while the graph re-prepares.
struct Parked {
    id: CrossfadeId,
    node: NodeKey,
    swap: Box<dyn ErasedSwap>,
    fade: Seconds,
    curve: CrossfadeCurve,
}

impl std::fmt::Debug for Parked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Parked")
            .field("id", &self.id)
            .field("node", &self.node)
            .field("fade", &self.fade)
            .field("curve", &self.curve)
            .finish_non_exhaustive()
    }
}

/// What a parked crossfade came to on a retry.
pub(crate) enum Retried {
    Landed {
        node: NodeKey,
        id: CrossfadeId,
        controls: Box<dyn Any + Send>,
    },
    Failed {
        node: NodeKey,
        id: CrossfadeId,
        refused: CrossfadeRefused,
    },
}

/// The editable audio graph. See the module docs.
///
/// Every method runs on the control side, never on the audio thread; they
/// may allocate. Edits mark the graph edited, and the session's next
/// [`update`](crate::Session::update) commits them, all at once.
///
/// # Why opaque
///
/// Every method is named in graph terms (insert a node, set a port's source,
/// replace a node under a fade) and takes an [`NodeKey`] and a
/// [`GraphSource`], never a runtime type. There is no access to the
/// `Editor`: the edit/commit boundary stays visible.
pub struct Graph {
    editor: Editor,
    local: Option<Local>,
    nodes: BTreeMap<NodeKey, Entry>,
    /// Edited since the last commit: what `update()` commits on, and what a
    /// local render commits first.
    edited: bool,
    /// Plans sent, for a host (or a test) asking whether an update sent one.
    commits: u64,
    /// Threads per block, the caller included (`0` for one per core).
    render_workers: usize,
    /// Run by each live helper on its own thread, before it takes work.
    render_worker_start: Option<RenderWorkerStart>,
    /// Each sink's event input 0, by feeder: the sources each feeder feeds
    /// it. Written into the spec as their union.
    feeds: BTreeMap<NodeKey, BTreeMap<Feeder, Vec<EventSource>>>,
    /// Each fed sink's event input 0: the sources on it that the feeds put
    /// there and the host did not wire itself, which are all a change of the
    /// feeds may take away. Every other edge on the input is the host's.
    fed_edges: BTreeMap<NodeKey, Vec<EventSource>>,
    /// The id the next tap takes.
    next_tap: u32,
    /// The id the next event tap takes.
    next_event_tap: u32,
    /// Each open event tap as it was opened (its port, alignment and
    /// capacity), so [`reopen_event_taps`](Self::reopen_event_taps) can open it
    /// again.
    event_taps: BTreeMap<EventTapId, (EventSource, TapAlign, usize)>,
    /// The taps that copy the graph's outputs ([`open_output_tap`](Self::open_output_tap)),
    /// by the channels they were opened with: their sources follow the outputs.
    output_taps: BTreeMap<TapId, usize>,
    /// The id the next meter takes (`MASTER_METER` is taken at construction).
    next_meter: u32,
    /// The master meter's reader until a host takes it.
    master_meter: Option<MeterReader>,
    /// Crossfades refused `Busy`, in request order.
    parked: VecDeque<Parked>,
    next_crossfade: u64,
    /// What the root's channels are, when a host declared it
    /// ([`Session::declare_root_topology`](crate::Session::declare_root_topology));
    /// `None` is the engine's order for the root's width.
    root: Option<tutti_types::ChannelTopology>,
    /// The control side of the engine's output folds, once an engine was built
    /// over this graph: a declaration is published through it.
    output: Option<tutti_engine::OutputControl>,
    /// The last widening [`widen_outputs`](Self::widen_outputs) refused.
    widen_refusal: Option<tutti_types::FoldError>,
    /// The hosted plugins' records, by node (`plugin.rs`): kept here so a
    /// crossfade to another plugin and a removal update them where they
    /// happen.
    #[cfg(feature = "plugin")]
    pub(crate) plugins: BTreeMap<NodeKey, crate::HostedPlugin>,
    /// A reader of the session's tempo map, the template each plugin
    /// inserted through the graph is bound with (a clone of it: the plugin
    /// node lends the map once per block and a fork clones its reader on the
    /// control side). The publisher is the session's `TransportSettings`,
    /// whose `Reclaim` sweep already covers the cell.
    #[cfg(feature = "plugin")]
    pub(crate) tempo: tutti_transport::TempoReader,
}

/// A hook each live render helper runs once on its own thread, with its
/// index (`1..`), before it takes work: see
/// [`SessionBuilder::render_worker_start`](crate::SessionBuilder::render_worker_start).
#[derive(Clone)]
pub struct RenderWorkerStart(Arc<dyn Fn(usize) + Send + Sync>);

impl RenderWorkerStart {
    /// Wrap `hook`.
    pub fn new(hook: impl Fn(usize) + Send + Sync + 'static) -> Self {
        Self(Arc::new(hook))
    }

    /// Run the hook for helper `index`.
    pub fn call(&self, index: usize) {
        (self.0)(index);
    }
}

impl std::fmt::Debug for RenderWorkerStart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RenderWorkerStart").finish_non_exhaustive()
    }
}

impl std::fmt::Debug for Graph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("Graph");
        d.field("editor", &self.editor)
            .field("edited", &self.edited)
            .field("commits", &self.commits)
            .field("render_workers", &self.render_workers)
            .field("feeds", &self.feeds)
            .field("fed_edges", &self.fed_edges)
            .field("parked", &self.parked)
            .field("next_crossfade", &self.next_crossfade);
        #[cfg(feature = "plugin")]
        d.field("plugins", &self.plugins);
        d.finish_non_exhaustive()
    }
}

/// What a commit came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Committed {
    /// Sent.
    Sent,
    /// Refused for good (logged): the same spec fails the same way, so
    /// nothing to retry until the next edit.
    Refused,
    /// Refused for now — the executor has not drained enough commits, or a
    /// re-prepare is between its halves. The edit flag stays set, and the
    /// next update retries.
    Retry,
}

/// The master meter's id; every other meter takes the ids after it.
const MASTER_METER: MeterId = MeterId(0);

fn compensation_of(plan: &tutti_graph::Plan) -> Compensation {
    Compensation::new(plan.compensation().to_vec(), plan.total_latency().samples())
}

/// The graph's `Prepare`: `rate`, `max_block`, and the device's callback
/// `quantum` when the host knows it (`tutti_cpal::OutputSpec::quantum`). A
/// hosted out-of-process plugin ships one quantum per callback to its server,
/// so the quantum is what keeps its pipeline in step with the device.
pub(crate) fn prepare_for(
    rate: SampleRate,
    max_block: Samples,
    quantum: Option<Samples>,
) -> Prepare {
    let p = Prepare::new(rate, max_block);
    match quantum {
        Some(q) => p.with_quantum(q),
        None => p,
    }
}

/// `latency` clamped to what PDC compensates, as the editor clamps a latency
/// it probes at insert.
#[cfg(feature = "plugin")]
fn clamp_latency(latency: Latency) -> Latency {
    Latency::new(latency.samples().min(tutti_graph::MAX_NODE_LATENCY))
}

impl Graph {
    /// An empty graph with `inputs` global inputs and `outputs` global
    /// outputs, prepared for `rate`, `max_block` and the device's callback
    /// `quantum` (when known), its executor local.
    pub(crate) fn new(
        inputs: ChannelLayout,
        outputs: usize,
        rate: SampleRate,
        max_block: Samples,
        quantum: Option<Samples>,
        tempo: tutti_transport::TempoReader,
    ) -> Self {
        #[cfg(not(feature = "plugin"))]
        let _ = tempo;
        let (mut editor, exec) = Editor::new(prepare_for(rate, max_block, quantum));
        let topology = &mut editor.spec_mut().topology;
        topology.inputs = inputs;
        topology.outputs = vec![Source::Zero; outputs];
        // Declared and on from the start: a host reads the master level
        // without asking for it.
        // `Outputs` names no source, so the open cannot be refused.
        let master_meter = editor
            .open_meter(MASTER_METER, MeterSpec::Outputs)
            .expect("a master meter names no source");
        master_meter.enable();
        Self {
            editor,
            local: Some(Local {
                exec,
                scratch: Vec::new(),
            }),
            nodes: BTreeMap::new(),
            edited: true,
            commits: 0,
            render_workers: 1,
            render_worker_start: None,
            feeds: BTreeMap::new(),
            fed_edges: BTreeMap::new(),
            next_tap: 0,
            next_event_tap: 0,
            event_taps: BTreeMap::new(),
            output_taps: BTreeMap::new(),
            next_meter: 1,
            master_meter: Some(master_meter),
            parked: VecDeque::new(),
            next_crossfade: 0,
            root: None,
            output: None,
            widen_refusal: None,
            #[cfg(feature = "plugin")]
            plugins: BTreeMap::new(),
            #[cfg(feature = "plugin")]
            tempo,
        }
    }

    /// What every node is prepared for: the rate, the largest block, and the
    /// device's callback size when the session knows it.
    pub fn prepared(&self) -> Prepare {
        *self.editor.prepare()
    }

    pub(crate) fn set_render_workers(&mut self, workers: usize) {
        self.render_workers = workers;
    }

    pub(crate) fn set_render_worker_start(&mut self, start: Option<RenderWorkerStart>) {
        self.render_worker_start = start;
    }

    /// A fresh pool of the configured size, its threads named `name-<i>`, or
    /// `None` for one participant or when the system refuses a thread
    /// (render on the caller alone). Each helper runs `start` first.
    pub(crate) fn render_pool(
        &self,
        name: &str,
        start: Option<&RenderWorkerStart>,
    ) -> Option<Arc<dyn tutti_graph::Pool>> {
        let n = match self.render_workers {
            0 => std::thread::available_parallelism().map_or(1, |n| n.get()),
            n => n,
        };
        if n < 2 {
            return None;
        }
        let name = name.to_owned();
        let mut builder = tutti_graph::WorkerPool::builder()
            .num_threads(n)
            .thread_name(move |i| format!("{name}-{i}"));
        if let Some(start) = start {
            let start = start.clone();
            builder = builder.start_handler(move |i| start.call(i));
        }
        let pool = builder
            .build()
            .inspect_err(|e| {
                tracing::warn!("graph: a {n}-thread worker pool was refused, rendering alone: {e}");
            })
            .ok()?;
        Some(Arc::new(pool))
    }

    /// Hand the executor over: to the engine, or to a test's audio side.
    fn take_executor(&mut self) -> Executor {
        self.local
            .take()
            .expect("the graph's audio side was already taken")
            .exec
    }

    /// The audio thread's half, for a test that renders what a device would
    /// hear: every commit lands on it, and ticking it plays the graph.
    ///
    /// # Panics
    ///
    /// If the audio side was already taken — by an earlier call, or by the
    /// engine the session builder built over it.
    pub fn take_audio_side(&mut self) -> AudioSide {
        AudioSide::live(self.take_executor())
    }

    /// Build the engine over this graph's audio side, which it takes:
    /// `Engine::new` over the editor and its executor, which bounds the
    /// editor to what the engine can render.
    pub(crate) fn engine(
        &mut self,
        transport: &tutti_transport::Transport,
    ) -> Result<tutti_engine::Engine, tutti_engine::GraphEngineError> {
        let mut exec = self.take_executor();
        exec.set_pool(self.render_pool("tutti-worker", self.render_worker_start.as_ref()));
        let engine = tutti_engine::Engine::new(transport, &mut self.editor, exec)?;
        self.output = Some(engine.output_control());
        Ok(engine)
    }

    /// Re-prepare every node for `rate`, keeping the block and the quantum.
    /// With the executor local, both halves of the re-prepare run here, now;
    /// with it taken, the second half lands on a later collect (and commits
    /// are `Retry` until it does). A refusal is logged.
    ///
    /// For a detached graph, which has no device to take a rate from. A
    /// device's session re-rates with [`Session::restart`](crate::Session).
    pub fn set_sample_rate(&mut self, rate: SampleRate) {
        let current = *self.editor.prepare();
        if let Err(e) = self.editor.reprepare(prepare_for(
            rate,
            current.max_block().samples(),
            current.quantum(),
        )) {
            tracing::error!("graph: re-prepare at {} Hz refused: {e}", rate.get());
            return;
        }
        self.pump_local();
    }

    /// Whether a re-prepare is between its two commits.
    pub fn is_repreparing(&self) -> bool {
        self.editor.is_repreparing()
    }

    /// Refuse, before a device restart stops anything, a re-prepare to
    /// `max_block` this graph could not start: past the engine's block
    /// capacity (the editor's limits, set by `Engine::new`), with one
    /// already between its halves, or on a poisoned editor.
    #[cfg(feature = "device")]
    pub(crate) fn check_reprepare(&self, max_block: Option<Samples>) -> Result<(), CommitError> {
        if let Some(cause) = self.editor.poison_cause() {
            return Err(CommitError::Poisoned {
                cause: cause.to_owned(),
            });
        }
        if self.editor.is_repreparing() {
            return Err(CommitError::Repreparing);
        }
        let limit = self.editor.limits().max_block;
        match max_block {
            Some(b) if b.get() > limit => Err(CommitError::BlockTooLong {
                max_block: b.get(),
                limit,
            }),
            _ => Ok(()),
        }
    }

    /// Re-prepare every node for `rate`, the device's callback `quantum` and,
    /// if given, `max_block` (else the block it has): the first half of
    /// `Editor::reprepare`, sent. A no-op when nothing moves.
    #[cfg(feature = "device")]
    pub(crate) fn reprepare(
        &mut self,
        rate: SampleRate,
        max_block: Option<Samples>,
        quantum: Option<Samples>,
    ) -> Result<(), CommitError> {
        let current = *self.editor.prepare();
        let prepare = prepare_for(
            rate,
            max_block.unwrap_or(current.max_block().samples()),
            quantum,
        );
        if prepare == current {
            return Ok(());
        }
        self.editor.reprepare(prepare)?;
        self.pump_local();
        Ok(())
    }

    /// Apply whatever the editor sent to a local executor, and collect what
    /// comes back, until nothing is in flight. A no-op once the executor is
    /// taken. Bounded: a re-prepare is two round trips, anything else one.
    fn pump_local(&mut self) {
        let Some(local) = &mut self.local else {
            return;
        };
        // The collect below frees what the local executor retired.
        crate::main_thread();
        for _ in 0..4 {
            if self.editor.in_flight() == 0 {
                break;
            }
            local.exec.apply_pending();
            self.editor.collect();
        }
    }

    // --- Nodes ---

    /// Adds `node` to the graph, unwired, through its own [`IntoNode`], and
    /// returns its handle and its controls. Its fork source goes to the
    /// editor, so an export forks it as its type says; it is prepared at the
    /// graph's rate. Address its params with
    /// [`set_node_params`](Self::set_node_params), or insert it with
    /// [`add`](Self::add), which does, and keeps its reclaimer
    /// ([`SessionNode::reclaimer`]): a node inserted here is not swept by
    /// the reclaim phase.
    pub fn insert<N: IntoNode>(&mut self, node: N) -> (NodeKey, N::Controls) {
        let id = NodeKey::fresh();
        let controls = self.editor.insert(id, N::kind(), node);
        self.nodes.insert(
            id,
            Entry {
                node: id,
                params: None,
                reclaimer: None,
            },
        );
        self.edited = true;
        (id, controls)
    }

    /// Opens an inlet: audio a thread of your own pushes into the graph. A
    /// ring of `capacity` frames and `width` channels is read by an
    /// [`InletNode`] (unwired; declare what it feeds like any node), and the
    /// writer is the producer's end: push whole frames with
    /// [`push_interleaved`](TapWriter::push_interleaved) from any one thread. The
    /// mirror of [`open_tap`](Self::open_tap), which copies the other way.
    ///
    /// An inlet **does not resample**, and its clock is not the graph's: the
    /// producer must run at the graph's rate, checked here, once, as
    /// `producer_rate`. After that two clocks drift however slowly, and
    /// nothing corrects it: a producer ahead of the graph finds the ring full
    /// and its writer refuses whole frames, counting them; one behind leaves
    /// the ring empty and the node writes silence for the frames it lacks.
    /// Neither side blocks. A ring of a few blocks keeps both rare, at that
    /// much latency. A re-prepare to another rate later is not checked.
    ///
    /// The node is unforkable: an export of a graph that holds it is refused,
    /// naming its key.
    ///
    /// # Errors
    ///
    /// [`InletError::RateMismatch`] when `producer_rate` is not the graph's
    /// rate; nothing is opened.
    pub fn open_inlet(
        &mut self,
        width: ChannelLayout,
        capacity: Samples,
        producer_rate: SampleRate,
    ) -> Result<(NodeKey, TapWriter), InletError> {
        let (ring, writer) = TapReader::standalone(TapId(0), usize::from(width.count()), capacity);
        Ok((self.add_inlet(ring, producer_rate)?, writer))
    }

    /// [`open_inlet`](Self::open_inlet) over a ring the producer's layer
    /// already built ([`TapReader::standalone`]), for a producer that must
    /// choose its width and rate before the graph sees it (a capture device).
    ///
    /// # Errors
    ///
    /// [`InletError::RateMismatch`] as for [`open_inlet`](Self::open_inlet).
    pub fn add_inlet(
        &mut self,
        ring: TapReader,
        producer_rate: SampleRate,
    ) -> Result<NodeKey, InletError> {
        let graph = self.editor.prepare().sample_rate();
        if producer_rate != graph {
            return Err(InletError::RateMismatch {
                producer: producer_rate,
                graph,
            });
        }
        Ok(self.insert(InletNode::new(ring)).0)
    }

    /// [`insert`](Self::insert) a [`SessionNode`], its params addressed by
    /// [`SessionNode::params`] and its reclaimer kept
    /// ([`SessionNode::reclaimer`]).
    pub fn add<N: SessionNode>(&mut self, node: N) -> (NodeKey, N::Controls) {
        let (id, controls) = self.insert(node);
        self.address::<N>(id, &controls);
        (id, controls)
    }

    /// Keeps what a [`SessionNode`] says of its controls under `node`: its
    /// params and its reclaimer.
    fn address<N: SessionNode>(&mut self, node: NodeKey, controls: &N::Controls) {
        if let Some(entry) = self.nodes.get_mut(&node) {
            entry.params = N::params(controls);
            entry.reclaimer = N::reclaimer(controls);
        }
    }

    /// Inserts a hosted plugin: bound (`PluginClient::bind`, its typestate
    /// transition to a graph node, with a reader of the session's tempo map,
    /// so the plugin is told the project's tempo) and inserted as the `Node` it then is,
    /// which hands the editor its own fork source (a fork by state transfer)
    /// and declares its latency in its `Shape`. Capture its controls first:
    /// the plugin is out of reach once inserted.
    #[cfg(feature = "plugin")]
    pub fn insert_plugin(&mut self, client: Box<tutti_plugin::handles::PluginClient>) -> NodeKey {
        let node = NodeKey::fresh();
        let hosted =
            crate::HostedPlugin::new(tutti_plugin::handles::PluginHandle::from_client(&client))
                .with_controls(client.controls());
        let tempo = self.tempo.clone();
        let _controls = self.editor.insert(node, PLUGIN_KIND, client.bind(tempo));
        self.nodes.insert(
            node,
            Entry {
                node,
                params: None,
                reclaimer: None,
            },
        );
        self.plugins.insert(node, hosted);
        self.edited = true;
        node
    }

    /// Hosts a plugin the host inserted itself (an in-process plugin, a
    /// wrapper around one): `handle` gets the session's health watch,
    /// snapshots and render mode at `node`, as
    /// [`Session::add_plugin`](crate::Session::add_plugin)'s do. Its latency
    /// is whatever its node's shape declared at insert. Returns `false`
    /// (and hosts nothing) when `node` is not in the graph.
    ///
    /// A host that binds a plugin itself (`PluginClient::bind`) gives it a
    /// reader of the session's tempo map, `session.transport().settings
    /// .tempo_reader()`, so the plugin is told the project's tempo; this
    /// method binds nothing.
    #[cfg(feature = "plugin")]
    pub fn host_plugin(
        &mut self,
        node: NodeKey,
        handle: tutti_plugin::handles::PluginHandle,
    ) -> bool {
        if !self.nodes.contains_key(&node) {
            return false;
        }
        self.plugins.insert(node, crate::HostedPlugin::new(handle));
        true
    }

    /// Swap the plugin at `node` for `client`, as [`replace`](Self::replace)
    /// swaps any node: crossfading when the running node's shape fits the
    /// bound plugin's, else a plain swap at the same key. Refused, handing
    /// `client` back unbound, while a re-prepare is between its two commits.
    ///
    /// # Errors
    ///
    /// As [`replace`](Self::replace): [`ReplaceRefused::Busy`] while the graph
    /// re-prepares, with `client` handed back unbound;
    /// [`ReplaceRefused::Failed`] on a poisoned graph or a `node` not in it.
    #[cfg(feature = "plugin")]
    pub fn replace_plugin(
        &mut self,
        node: NodeKey,
        client: Box<tutti_plugin::handles::PluginClient>,
        fade: Seconds,
        curve: CrossfadeCurve,
    ) -> Result<(), ReplaceRefused<Box<tutti_plugin::handles::PluginClient>>> {
        // Asked here, before binding: `replace` hands back what it refuses,
        // and a bound plugin cannot be unbound.
        if let Some(cause) = self.editor.poison_cause() {
            return Err(ReplaceRefused::Failed(CrossfadeRefused::Poisoned(
                cause.to_owned(),
            )));
        }
        if self.editor.is_repreparing() {
            return Err(ReplaceRefused::Busy(client));
        }
        let handle = tutti_plugin::handles::PluginHandle::from_client(&client);
        let controls = client.controls();
        let tempo = self.tempo.clone();
        match self.swap_unit(node, client.bind(tempo), fade, curve) {
            Ok(_controls) => {
                // The record is carried over to the incoming plugin (unlike
                // `replace`, which takes it out): its handle is what the
                // health watch and the render mode talk to now, its controls
                // what the latency poll reads; nothing of the outgoing
                // plugin's state survives (`HostedPlugin::replace`).
                match self.plugins.get_mut(&node) {
                    Some(hosted) => hosted.replace(handle, controls),
                    None => {
                        self.plugins.insert(
                            node,
                            crate::HostedPlugin::new(handle).with_controls(controls),
                        );
                    }
                }
                Ok(())
            }
            Err(ReplaceRefused::Failed(refused)) => Err(ReplaceRefused::Failed(refused)),
            Err(ReplaceRefused::Busy(_)) => unreachable!("checked above"),
        }
    }

    /// The control-thread half of a copy of `target` for an offline render
    /// at `rate`, sharing no state with this graph: `Editor::fork_gather`
    /// with `ForkMode::Offline`, through tutti-export
    /// (`RenderGraph::gather`). [`PendingExport::build`] makes the rest on a
    /// worker.
    ///
    /// # Errors
    ///
    /// What `RenderGraph::gather` refuses (a node with no outputs or not in
    /// the graph, a node that cannot be forked).
    #[cfg(feature = "export")]
    pub(crate) fn gather_for_export(
        &self,
        target: AudioNodeTarget,
        rate: SampleRate,
    ) -> tutti_export::Result<PendingExport> {
        // Only the master's outputs are the root's channels; a node's
        // sub-graph has its own, undeclared.
        let root = match target {
            AudioNodeTarget::Master => self.root.clone(),
            AudioNodeTarget::Node(_) => None,
        };
        let target = match target {
            AudioNodeTarget::Master => tutti_graph::ForkTarget::Master,
            AudioNodeTarget::Node(node) => tutti_graph::ForkTarget::Node(node),
        };
        let fork = tutti_export::RenderGraph::gather(
            &self.editor,
            target,
            tutti_graph::ForkMode::Offline,
            rate,
        )?;
        Ok(PendingExport {
            fork,
            // Its own pool, not the live engine's: the live callback would
            // find a shared one busy with the export's job and render alone.
            // No start hook: an export's threads are not the callback's, and
            // promoting them would take cores from it.
            pool: self.render_pool("tutti-export", None),
            root,
        })
    }

    /// Runs every node's reclaimer ([`SessionNode::reclaimer`], kept by
    /// [`add`](Self::add) and [`crossfade`](Self::crossfade), dropped with
    /// the node) and (`plugin`) reclaims every hosted plugin's meter cell,
    /// so a cell that stopped publishing still has what it retired freed.
    /// Returns how many retired values are still pinned. Control thread:
    /// the session's [`Reclaim`](crate::Phase::Reclaim) phase.
    pub(crate) fn reclaim(&self) -> usize {
        let pinned: usize = self
            .nodes
            .values()
            .filter_map(|entry| entry.reclaimer.as_ref())
            .map(crate::Reclaimer::reclaim)
            .sum();
        // The engine's output folds (offline or on a device): the declaration
        // retires a set, and a block may hold it when it does. Not observable
        // from outside the engine (a held loan is the audio thread's); the
        // cell's own behaviour is pinned in `tutti-engine`
        // (`a_replaced_fold_set_is_reclaimed_after_the_block`).
        let pinned = pinned
            + self
                .output
                .as_ref()
                .map_or(0, tutti_engine::OutputControl::reclaim);
        #[cfg(feature = "plugin")]
        let pinned = pinned
            + self
                .plugins
                .values()
                .filter_map(crate::HostedPlugin::meter)
                .map(tutti_plugin::handles::PluginMeter::reclaim)
                .sum::<usize>();
        pinned
    }

    /// Whether the graph keeps a reclaimer for `node`
    /// ([`SessionNode::reclaimer`]): for a test that a path inserts a node
    /// through [`add`](Self::add). Not API.
    #[doc(hidden)]
    pub fn keeps_reclaimer(&self, node: NodeKey) -> bool {
        self.nodes
            .get(&node)
            .is_some_and(|entry| entry.reclaimer.is_some())
    }

    /// Addresses `node`'s params by `params` (a `ParamNode`'s controls): what
    /// [`set_param`](Self::set_param) writes through for it, and what a fork
    /// of it starts from. `None` takes the address away.
    pub fn set_node_params(&mut self, node: NodeKey, params: Option<ParamSet>) {
        if let Some(entry) = self.nodes.get_mut(&node) {
            entry.params = params;
        }
    }

    /// Swaps the unit behind `node` for `incoming` under a `fade` along
    /// `curve` when its shape fits the running unit's, else as a plain swap
    /// on the next commit (`Editor::replace_or_swap`). Hands back its
    /// controls; the caller addresses its params
    /// ([`set_node_params`](Self::set_node_params)). The outgoing node's
    /// reclaimer goes with it and the incoming node keeps none
    /// ([`crossfade`](Self::crossfade) keeps a [`SessionNode`]'s). Refused as
    /// [`ReplaceRefused::Busy`] with the node handed back while the graph
    /// re-prepares, and for good on a poisoned graph.
    /// [`crossfade`](Self::crossfade) waits out a re-prepare instead.
    ///
    /// (`plugin`) A hosted plugin replaced this way is no longer hosted: its
    /// record is dropped, so its health, snapshots and render mode stop
    /// (a replacement plugin goes in with
    /// `Graph::replace_plugin`, which carries the record).
    ///
    /// # Errors
    ///
    /// [`ReplaceRefused::Busy`] while the graph re-prepares, handing `incoming`
    /// back; [`ReplaceRefused::Failed`] on a poisoned graph or a `node` not in
    /// it, dropping `incoming`.
    pub fn replace<N: IntoNode>(
        &mut self,
        node: NodeKey,
        incoming: N,
        fade: Seconds,
        curve: CrossfadeCurve,
    ) -> Result<N::Controls, ReplaceRefused<N>> {
        let controls = self.swap_unit(node, incoming, fade, curve)?;
        #[cfg(feature = "plugin")]
        self.plugins.remove(&node);
        Ok(controls)
    }

    /// [`replace`](Self::replace)'s swap, leaving the plugin record alone.
    fn swap_unit<N: IntoNode>(
        &mut self,
        node: NodeKey,
        incoming: N,
        fade: Seconds,
        curve: CrossfadeCurve,
    ) -> Result<N::Controls, ReplaceRefused<N>> {
        if let Some(cause) = self.editor.poison_cause() {
            return Err(ReplaceRefused::Failed(CrossfadeRefused::Poisoned(
                cause.to_owned(),
            )));
        }
        if self.editor.is_repreparing() {
            return Err(ReplaceRefused::Busy(incoming));
        }
        if !self.nodes.contains_key(&node) {
            return Err(ReplaceRefused::Failed(CrossfadeRefused::NoSuchNode(node)));
        }
        let rate = self.editor.prepare().sample_rate();
        let controls = self
            .editor
            .replace_or_swap(node, incoming, Fade::seconds(fade, rate, curve))
            .map_err(|e| ReplaceRefused::Failed(CrossfadeRefused::Editor(e)))?;
        if let Some(entry) = self.nodes.get_mut(&node) {
            entry.params = None;
            entry.reclaimer = None;
        }
        self.edited = true;
        Ok(controls)
    }

    /// Swaps the node behind `node` for `incoming` under `fade` along
    /// `curve`, as [`replace`](Self::replace) does, addressing the incoming
    /// node's params ([`SessionNode::params`]) — and, while the graph
    /// re-prepares, keeps it waiting instead of handing it back: the first
    /// update that finds the graph resumed swaps it in
    /// ([`Crossfade::Waiting`]). A crossfade asked for while another of the
    /// same node waits waits behind it, so the later one still wins.
    ///
    /// # Errors
    ///
    /// On a poisoned graph, or a node not in it: the incoming node is dropped.
    pub fn crossfade<N>(
        &mut self,
        node: NodeKey,
        incoming: N,
        fade: Seconds,
        curve: CrossfadeCurve,
    ) -> Result<Crossfade<N::Controls>, CrossfadeRefused>
    where
        N: SessionNode,
        N::Controls: Send + 'static,
    {
        if self.parked.iter().any(|p| p.node == node) {
            return Ok(Crossfade::Waiting(self.park(
                node,
                Box::new(Swap(incoming)),
                fade,
                curve,
            )));
        }
        match self.replace(node, incoming, fade, curve) {
            Ok(controls) => {
                self.address::<N>(node, &controls);
                Ok(Crossfade::Landed(controls))
            }
            Err(ReplaceRefused::Busy(n)) => Ok(Crossfade::Waiting(self.park(
                node,
                Box::new(Swap(n)),
                fade,
                curve,
            ))),
            Err(ReplaceRefused::Failed(refused)) => Err(refused),
        }
    }

    /// [`crossfade`](Self::crossfade) for a hosted plugin: the swap is
    /// [`replace_plugin`](Self::replace_plugin)'s, and a landing reports `()`
    /// as its controls (capture the plugin's before handing it over).
    ///
    /// # Errors
    ///
    /// As `crossfade`.
    #[cfg(feature = "plugin")]
    pub fn crossfade_plugin(
        &mut self,
        node: NodeKey,
        client: Box<tutti_plugin::handles::PluginClient>,
        fade: Seconds,
        curve: CrossfadeCurve,
    ) -> Result<Crossfade<()>, CrossfadeRefused> {
        if self.parked.iter().any(|p| p.node == node) {
            return Ok(Crossfade::Waiting(self.park(
                node,
                Box::new(PluginSwap(client)),
                fade,
                curve,
            )));
        }
        match self.replace_plugin(node, client, fade, curve) {
            Ok(()) => Ok(Crossfade::Landed(())),
            Err(ReplaceRefused::Busy(c)) => Ok(Crossfade::Waiting(self.park(
                node,
                Box::new(PluginSwap(c)),
                fade,
                curve,
            ))),
            Err(ReplaceRefused::Failed(refused)) => Err(refused),
        }
    }

    fn park(
        &mut self,
        node: NodeKey,
        swap: Box<dyn ErasedSwap>,
        fade: Seconds,
        curve: CrossfadeCurve,
    ) -> CrossfadeId {
        let id = CrossfadeId(self.next_crossfade);
        self.next_crossfade += 1;
        self.parked.push_back(Parked {
            id,
            node,
            swap,
            fade,
            curve,
        });
        id
    }

    /// How many crossfades wait for a re-prepare to resume.
    pub fn waiting_crossfades(&self) -> usize {
        self.parked.len()
    }

    /// Try every waiting crossfade again, in request order: one still `Busy`
    /// goes back on the queue, in order.
    pub(crate) fn retry_crossfades(&mut self) -> Vec<Retried> {
        let mut out = Vec::new();
        for Parked {
            id,
            node,
            swap,
            fade,
            curve,
        } in std::mem::take(&mut self.parked)
        {
            if self.parked.iter().any(|p| p.node == node) {
                // An earlier one of the same node is still waiting.
                self.parked.push_back(Parked {
                    id,
                    node,
                    swap,
                    fade,
                    curve,
                });
                continue;
            }
            match swap.swap(self, node, fade, curve) {
                Ok(controls) => out.push(Retried::Landed { node, id, controls }),
                Err(ReplaceRefused::Busy(swap)) => self.parked.push_back(Parked {
                    id,
                    node,
                    swap,
                    fade,
                    curve,
                }),
                Err(ReplaceRefused::Failed(refused)) => {
                    tracing::error!("graph: crossfade of {node:?} refused: {refused}");
                    out.push(Retried::Failed { node, id, refused });
                }
            }
        }
        out
    }

    /// How many event inputs `node` declares.
    pub fn node_event_inputs(&self, node: NodeKey) -> usize {
        self.shape(node).map_or(0, |s| usize::from(s.event_in))
    }

    /// Feeds `sink`'s event input `port` from exactly `sources` (fan-in
    /// merges them by offset; the graph orders ties by source), beside what
    /// the feeds ([`set_feed`](Self::set_feed)) feed it on input 0: the
    /// feeds are partial, as audio's declarations are, and own only their
    /// own sources. Touches nothing when that is what it already holds.
    /// Event edges from a node the graph did not insert through
    /// [`insert`](Self::insert) (a modulation lane on a plugin's event
    /// input) are not the caller's, and stay.
    pub fn set_event_sources(&mut self, sink: NodeKey, port: u16, sources: &[EventSource]) {
        if port != 0 {
            self.write_event_sources(sink, port, sources);
            return;
        }
        let fed = self.fed_in_graph(sink);
        let theirs: Vec<EventSource> = fed.into_iter().filter(|s| !sources.contains(s)).collect();
        let mut all = sources.to_vec();
        all.extend_from_slice(&theirs);
        self.set_fed_edges(sink, theirs);
        self.write_event_sources(sink, 0, &all);
    }

    /// Feeds `sink`'s event input `port` from exactly `sources`, but for
    /// edges from nodes the graph did not insert.
    fn write_event_sources(&mut self, sink: NodeKey, port: u16, sources: &[EventSource]) {
        let at = EventIn { node: sink, port };
        let mut want: Vec<EventOut> = sources
            .iter()
            .map(|s| EventOut {
                node: s.node,
                port: s.port,
            })
            .collect();
        want.sort();
        want.dedup();
        let nodes = &self.nodes;
        let spec = self.editor.spec_mut();
        let (have, others): (Vec<EventEdge>, Vec<EventEdge>) = spec
            .events
            .get(&at)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .partition(|e| nodes.contains_key(&e.from().node));
        let have: Vec<EventOut> = have.iter().map(|e| e.from()).collect();
        if have == want {
            return;
        }
        spec.events.remove(&at);
        for edge in others {
            spec.connect_events(at, edge);
        }
        for from in want {
            spec.connect_events(at, EventEdge::Direct(from));
        }
        self.edited = true;
    }

    /// The event outputs feeding `sink`'s event input `port`, in the graph's
    /// order: every edge the spec holds, a modulation lane's included.
    pub fn event_sources(&self, sink: NodeKey, port: u16) -> Vec<EventSource> {
        let at = EventIn { node: sink, port };
        self.editor
            .spec()
            .events
            .get(&at)
            .into_iter()
            .flatten()
            .map(|e| {
                let from = e.from();
                EventSource::new(from.node, from.port)
            })
            .collect()
    }

    // --- Event feeds ---

    /// Sets what `feeder` feeds `sink`'s event input 0, replacing what it fed
    /// before (empty: nothing). The sink's input reads the union of every
    /// feeder's sources that are in the graph, written when it changes; a
    /// sink with no event input takes nothing, and keeps the feed.
    ///
    /// Each [`Feeder`] owns only its own entry, so feeders never overwrite
    /// each other.
    pub fn set_feed(&mut self, sink: NodeKey, feeder: Feeder, sources: Vec<EventSource>) {
        if sources.is_empty() {
            // No entry is made for a feeder that feeds nothing.
            let Some(feeds) = self.feeds.get_mut(&sink) else {
                return;
            };
            if feeds.remove(&feeder).is_none() {
                return;
            }
            if feeds.is_empty() {
                self.feeds.remove(&sink);
            }
        } else {
            let feeds = self.feeds.entry(sink).or_default();
            if feeds.get(&feeder) == Some(&sources) {
                return;
            }
            feeds.insert(feeder, sources);
        }
        self.write_feeds(sink);
    }

    /// What `feeder` feeds `sink`: empty when nothing.
    pub fn feed(&self, sink: NodeKey, feeder: Feeder) -> &[EventSource] {
        self.feeds
            .get(&sink)
            .and_then(|f| f.get(&feeder))
            .map_or(&[], Vec::as_slice)
    }

    /// Every source any feeder feeds `sink`, sorted and without repeats.
    pub fn fed(&self, sink: NodeKey) -> Vec<EventSource> {
        let mut all: Vec<EventSource> = self
            .feeds
            .get(&sink)
            .into_iter()
            .flat_map(|f| f.values().flatten().copied())
            .collect();
        all.sort_by_key(EventSource::key);
        all.dedup();
        all
    }

    /// Moves every feeder's sources from sink `from` to sink `to`, replacing
    /// what each fed `to`: a host whose sink was re-bound to a new node keeps
    /// its feeds. `from` is left fed by nothing.
    pub fn move_feeds(&mut self, from: NodeKey, to: NodeKey) {
        if from == to {
            return;
        }
        let Some(moved) = self.feeds.remove(&from) else {
            return;
        };
        let into = self.feeds.entry(to).or_default();
        for (feeder, sources) in moved {
            into.insert(feeder, sources);
        }
        self.write_feeds(from);
        self.write_feeds(to);
    }

    /// Write `sink`'s feeds into its event input 0: the union of every
    /// feeder's sources still in the graph, beside the edges the host wired
    /// there itself (every edge the feeds did not put there), which stay.
    fn write_feeds(&mut self, sink: NodeKey) {
        if !self.nodes.contains_key(&sink) || self.node_event_inputs(sink) == 0 {
            return;
        }
        let union = self.fed_in_graph(sink);
        let ours = self.fed_edges.get(&sink).cloned().unwrap_or_default();
        let mut all: Vec<EventSource> = self
            .event_sources(sink, 0)
            .into_iter()
            .filter(|s| self.nodes.contains_key(&s.node) && !ours.contains(s))
            .collect();
        let hand = all.len();
        let theirs: Vec<EventSource> = union
            .into_iter()
            .filter(|s| !all[..hand].contains(s))
            .collect();
        all.extend_from_slice(&theirs);
        self.set_fed_edges(sink, theirs);
        self.write_event_sources(sink, 0, &all);
    }

    /// Every source any feeder feeds `sink` that is in the graph.
    fn fed_in_graph(&self, sink: NodeKey) -> Vec<EventSource> {
        self.fed(sink)
            .into_iter()
            .filter(|s| self.contains(s.node))
            .collect()
    }

    /// Records `sources` as the edges the feeds own on `sink`'s input 0.
    fn set_fed_edges(&mut self, sink: NodeKey, sources: Vec<EventSource>) {
        if sources.is_empty() {
            self.fed_edges.remove(&sink);
        } else {
            self.fed_edges.insert(sink, sources);
        }
    }

    /// Takes `node` out of the graph. Every edge to and from it reads silence
    /// afterwards, so a sink still naming it is left silent rather than
    /// dangling, and the feeds into and from it go.
    ///
    /// Returns whether it was there; removing a node twice is a no-op.
    pub fn remove(&mut self, node: NodeKey) -> bool {
        if self.nodes.remove(&node).is_none() {
            return false;
        }
        #[cfg(feature = "plugin")]
        self.plugins.remove(&node);
        self.editor.remove(node);
        self.feeds.remove(&node);
        for feeds in self.feeds.values_mut() {
            for sources in feeds.values_mut() {
                sources.retain(|s| s.node != node);
            }
            feeds.retain(|_, s| !s.is_empty());
        }
        self.feeds.retain(|_, f| !f.is_empty());
        self.fed_edges.remove(&node);
        for sources in self.fed_edges.values_mut() {
            sources.retain(|s| s.node != node);
        }
        self.fed_edges.retain(|_, s| !s.is_empty());
        self.edited = true;
        true
    }

    /// How many nodes the graph holds.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Whether `node` is in the graph, a modulation lane or signal node
    /// included (staged, committed or not).
    pub fn contains(&self, node: NodeKey) -> bool {
        self.editor.spec().topology.nodes.contains_key(&node)
    }

    /// Applies `decl` to the editor through `m` (doc 014 §8.2), resolving the
    /// host's keys through `keys`. A native target's knob is its node's own
    /// `Param` cell, from the node's [`ParamSet`]. Marks the graph edited
    /// when the apply asks for a commit.
    ///
    /// # Errors
    ///
    /// The apply's refusal: the graph keeps what the last apply built.
    #[cfg(feature = "modulation")]
    pub(crate) fn modulate(
        &mut self,
        m: &mut tutti_nodes::Modulation,
        decl: &tutti_modulation::Declarations<'_>,
        keys: &dyn ModKeys,
    ) -> Result<(tutti_nodes::Applied, PluginTargets), tutti_nodes::ReconcileError> {
        let host = ModHostOver {
            nodes: &self.nodes,
            #[cfg(feature = "plugin")]
            plugins: &self.plugins,
            keys,
            plugin_targets: std::cell::RefCell::new(BTreeMap::new()),
        };
        let applied = m.apply(decl, &mut self.editor, &host)?;
        if applied.commit {
            self.edited = true;
        }
        Ok((applied, host.plugin_targets.into_inner()))
    }

    /// Schedules the note-envelope resyncs `m` owes, after the commit that
    /// sent their nodes (`Modulation::send_resyncs`).
    #[cfg(feature = "modulation")]
    pub(crate) fn send_mod_resyncs(&mut self, m: &mut tutti_nodes::Modulation) {
        // A resync refused for now stays owed (the next update retries); one
        // refused for good is counted (`Modulation::resyncs_dropped`).
        if let Err(e) = m.send_resyncs(&mut self.editor) {
            tracing::debug!("modulation: a resync waits: {e}");
        }
        self.pump_local();
    }

    /// Writes `value` to `node`'s scalar param `param`, through the node's
    /// [`ParamSet`] (its controls, addressed with
    /// [`set_node_params`](Self::set_node_params)): the cell it reads at the
    /// start of its next block, and the value a fork starts from. A node
    /// without the param takes nothing. `param` is a [`UnitAddr`] (a kind and
    /// an index among the node's params of that kind); a bare `UnitParam` is
    /// index 0.
    pub fn set_param(&mut self, node: NodeKey, param: impl Into<UnitAddr>, value: f32) {
        if let Some(params) = self.nodes.get(&node).and_then(|e| e.params.as_ref()) {
            params.set(param, value);
        }
    }

    /// A live duplicate of the whole graph that shares no state with it
    /// (`Editor::fork`, `ForkMode::Live`), as an audio side to render. Each
    /// node is forked from its own fork source: a `ParamNode` from the
    /// values its set's cells hold. What an export renders from, so a test
    /// can check that a control write reaches it.
    ///
    /// # Errors
    ///
    /// When a node cannot be forked.
    pub fn fork_live(&self) -> Result<AudioSide, tutti_graph::ForkError> {
        let (editor, exec) = self.editor.fork(
            tutti_graph::ForkTarget::Master,
            tutti_graph::ForkMode::Live,
            *self.editor.prepare(),
        )?;
        Ok(AudioSide::forked(editor, exec))
    }

    // --- Edges ---

    fn lower(source: GraphSource) -> Source {
        match source {
            GraphSource::Node(node, port) => Source::Node(OutPort {
                node,
                port: port as u16,
            }),
            GraphSource::Input(port) => Source::Global(port as u16),
            GraphSource::Silence => Source::Zero,
        }
    }

    fn lift(&self, source: Source) -> GraphSource {
        match source {
            Source::Node(p) => self.nodes.get(&p.node).map_or(GraphSource::Silence, |e| {
                GraphSource::Node(e.node, p.port as usize)
            }),
            Source::Global(port) => GraphSource::Input(port as usize),
            Source::Zero => GraphSource::Silence,
        }
    }

    /// What feeds `node`'s input `port`.
    pub fn source(&self, node: NodeKey, port: usize) -> GraphSource {
        let at = InPort {
            node,
            port: port as u16,
        };
        match self.editor.spec().topology.edges.get(&at) {
            Some(Edge::Direct(source)) => self.lift(*source),
            // This graph writes no feedback edge.
            Some(Edge::Feedback(_)) | None => GraphSource::Silence,
        }
    }

    /// Feeds `node`'s input `port` from `source`. A port holds one source;
    /// this replaces whatever it held.
    ///
    /// # Panics
    ///
    /// If `source` is `node` itself: a node cannot feed its own input.
    pub fn set_source(&mut self, node: NodeKey, port: usize, source: GraphSource) {
        assert!(
            !matches!(source, GraphSource::Node(from, _) if from == node),
            "a node cannot feed its own input"
        );
        let at = InPort {
            node,
            port: port as u16,
        };
        let edges = &mut self.editor.spec_mut().topology.edges;
        match Self::lower(source) {
            // An unwritten port reads silence, so silence is no edge: the
            // value stays the same whether a port was never wired or wired
            // and cleared.
            Source::Zero => {
                edges.remove(&at);
            }
            source => {
                edges.insert(at, Edge::Direct(source));
            }
        }
        self.edited = true;
    }

    // --- Param modulation ---

    /// Whether `node` declares `param` modulatable (its shape's params).
    pub fn declares_param(&self, node: NodeKey, param: impl Into<UnitAddr>) -> bool {
        self.shape(node)
            .is_some_and(|s| s.params.index_of(param).is_some())
    }

    /// Drives `node`'s declared `param` from `source`'s event output 0
    /// (`None`: from nothing, so it reads its base), clamped to `range`:
    /// replaces whatever drove it. When the source is the one the graph
    /// already holds, only the range moves: that goes through
    /// `Editor::set_param_range`, a store the running node reads on its next
    /// block, and marks nothing for commit.
    ///
    /// # Errors
    ///
    /// Refused, touching nothing, for a NaN bound, with the error the commit
    /// would otherwise fail on (`GraphInvalid::BadParamRange`).
    pub fn set_param_mod(
        &mut self,
        node: NodeKey,
        param: impl Into<UnitAddr>,
        source: Option<NodeKey>,
        range: ParamRange,
    ) -> Result<(), GraphInvalid> {
        let at = ParamIn {
            node,
            param: param.into(),
        };
        if range.min.is_nan() || range.max.is_nan() {
            return Err(GraphInvalid::BadParamRange { at, range });
        }
        let want = source.map(|from| EventOut {
            node: from,
            port: 0,
        });
        let held = self.editor.spec().params.get(&at).map(|m| m.source);
        if want.is_some() && held == Some(want) {
            // A range move alone: a control, not a rewire.
            return self.editor.set_param_range(at, range);
        }
        let spec = self.editor.spec_mut();
        spec.params.remove(&at);
        spec.params.insert(
            at,
            ParamMod {
                range,
                source: want,
            },
        );
        self.edited = true;
        Ok(())
    }

    /// Stops modulating `node`'s `param`: it reads its own control again.
    pub fn clear_param_mod(&mut self, node: NodeKey, param: impl Into<UnitAddr>) {
        let at = ParamIn {
            node,
            param: param.into(),
        };
        if self.editor.spec_mut().params.remove(&at).is_some() {
            self.edited = true;
        }
    }

    /// How `node`'s `param` is modulated, as the graph value holds it.
    pub fn param_mod(&self, node: NodeKey, param: impl Into<UnitAddr>) -> Option<ParamMod> {
        self.editor
            .spec()
            .params
            .get(&ParamIn {
                node,
                param: param.into(),
            })
            .cloned()
    }

    /// What feeds global output `channel`.
    pub fn output_source(&self, channel: usize) -> GraphSource {
        self.editor
            .spec()
            .topology
            .outputs
            .get(channel)
            .map_or(GraphSource::Silence, |s| self.lift(*s))
    }

    /// Feeds global output `channel` from `source`.
    ///
    /// # Panics
    ///
    /// If `channel` is past the global outputs.
    pub fn set_output_source(&mut self, channel: usize, source: GraphSource) {
        self.editor.spec_mut().topology.outputs[channel] = Self::lower(source);
        self.follow_outputs();
        self.edited = true;
    }

    /// Feeds **every** global output from `node`: channel `c` from its port
    /// `c % node_outputs(node)`, or silence if it has no outputs.
    ///
    /// A node whose outputs are not all for the speakers (an instrument with
    /// effect sends, such as the SoundFont unit) wraps its sends onto a root
    /// wider than its main pair: use
    /// [`set_outputs_from_ports`](Self::set_outputs_from_ports) with the pair.
    pub fn set_outputs_from(&mut self, node: NodeKey) {
        let outs = self.node_outputs(node);
        self.feed_outputs(node, outs, |c| c % outs.max(1));
    }

    /// Feeds **every** global output from `node`, channel `c` from port
    /// `ports[c % ports.len()]`, or silence if `ports` is empty or names a port
    /// the node lacks. For an instrument, `ports` is its main pair
    /// (`InstrumentHandle::main_outputs`, feature `midi`),
    /// so a wide root never reaches its sends.
    pub fn set_outputs_from_ports(&mut self, node: NodeKey, ports: &[usize]) {
        let outs = self.node_outputs(node);
        let ports = ports.to_vec();
        let have = !ports.is_empty() && ports.iter().all(|&p| p < outs);
        self.feed_outputs(node, usize::from(have), |c| ports[c % ports.len()]);
    }

    fn feed_outputs(&mut self, node: NodeKey, usable: usize, port: impl Fn(usize) -> usize) {
        for (c, source) in self
            .editor
            .spec_mut()
            .topology
            .outputs
            .iter_mut()
            .enumerate()
        {
            *source = if usable == 0 {
                Source::Zero
            } else {
                Source::Node(OutPort {
                    node,
                    port: port(c) as u16,
                })
            };
        }
        self.follow_outputs();
        self.edited = true;
    }

    // --- Taps ---

    /// Copies `sources`, one lane each, out of the graph into a ring the
    /// returned reader drains: a tap on any ports, at any width. The tap is
    /// compiled by the next commit and fed by every block that renders after
    /// it (a device, [`Session::render`](crate::Session::render) and
    /// [`Session::render_frame`](crate::Session::render_frame) alike).
    /// `capacity` is each lane's ring in frames; what the reader does not
    /// drain in time is counted ([`TapReader::take_lost`]).
    ///
    /// [`TapAlign::ToOutput`] delays each lane by the compensation an output
    /// of the same source gets, so it lines up with what the outputs carry;
    /// [`TapAlign::Raw`] reads the source as it leaves its node.
    ///
    /// This is the **plan tap**: an op of the graph, so it sees the signal
    /// before the engine's declick fader and the fold to the device width, on
    /// any ports at any width. To record what the output sounds like, with the
    /// fade, open [`Session::open_heard_tap`](crate::Session::open_heard_tap)
    /// instead (the default for a recording); this is the explicit choice of
    /// the graph's own signal. A block the engine refuses (rendered silent)
    /// writes no frames here and counts none lost, since the op did not run;
    /// the heard tap writes the silence.
    ///
    /// **Because it is before the declick fader, at a transport stop it shows
    /// in a recording.** A transport-gated source (a
    /// clip, the click, a sampler voice) ends on the stop's frame, so the tap
    /// steps from the source's whole level to silence in one frame where the
    /// engine's output buffer has faded over the declick (measured: a 0.5 source gives
    /// a step of 0.5 in the tap and a largest step of about 0.001 in the
    /// output). A source that keeps running through the stop (live input, a
    /// tail) has none. A start steps alike in both: `Play` is not declicked.
    ///
    /// Every source must exist now, edits not yet committed included: a key
    /// no node has, a port past the node's width or an input past the graph's
    /// is refused with [`MissingSource`] and nothing is opened. Only the open
    /// is checked. A port that narrows or vanishes later (a node replaced by
    /// one with fewer outputs, a removed node) reads silence, the commit
    /// succeeds and the reader is not told. A latency
    /// change that resizes an aligned tap's delay shifts the stream with no
    /// marker for the reader, as it does an output. What the reader does not
    /// drain is dropped and counted, and a recording closes up over the gap.
    ///
    /// Dropping the reader leaves the tap open until
    /// [`close_tap`](Self::close_tap) or the session's next
    /// [`Reclaim`](crate::Phase::Reclaim) phase, which closes it.
    ///
    /// # Errors
    ///
    /// [`MissingSource`] for the first source that is not in the graph now.
    pub fn open_tap(
        &mut self,
        sources: &[GraphSource],
        align: TapAlign,
        capacity: Samples,
    ) -> Result<TapReader, MissingSource> {
        let sources = sources.iter().copied().map(Self::lower).collect();
        let id = TapId(self.next_tap);
        let reader = self.editor.open_tap(id, sources, align, capacity)?;
        self.next_tap += 1;
        self.edited = true;
        Ok(reader)
    }

    /// A tap on the graph's global outputs, aligned: lane `c` is output
    /// channel `c` as the outputs carry it. Its sources follow the outputs
    /// when they are re-wired ([`set_output_source`](Self::set_output_source),
    /// [`set_outputs_from`](Self::set_outputs_from)); its width is the
    /// output count when it was opened, and a later
    /// [`widen_outputs`](Self::widen_outputs) does not widen it. When the root
    /// **narrows** (a declared topology narrower than the root was), the lanes
    /// past the new width read silence, and [`take_lost`](TapReader::take_lost)
    /// does not count it: the reader keeps the lanes it opened with.
    ///
    /// This is the signal the graph produces, before the engine's declick
    /// fader and the fold to the device's width; see [`open_tap`](Self::open_tap)
    /// for what that means at a transport stop. To record what is heard, with
    /// the fade, use [`Session::open_heard_tap`](crate::Session::open_heard_tap).
    ///
    /// Refuses with [`MissingSource`] when an output names a source that does
    /// not exist now (an output wired to a port the node does not have).
    ///
    /// # Errors
    ///
    /// [`MissingSource`] for the first source that is not in the graph now.
    pub fn open_output_tap(&mut self, capacity: Samples) -> Result<TapReader, MissingSource> {
        let outputs: Vec<GraphSource> =
            (0..self.outputs()).map(|c| self.output_source(c)).collect();
        let reader = self.open_tap(&outputs, TapAlign::ToOutput, capacity)?;
        self.output_taps.insert(reader.id(), reader.width());
        Ok(reader)
    }

    /// Closes a tap: its ops leave the plan at the next commit and the ring
    /// is freed on this side. Closing a closed tap does nothing.
    pub fn close_tap(&mut self, id: TapId) {
        self.output_taps.remove(&id);
        self.editor.close_tap(id);
        self.edited = true;
    }

    /// Closes the taps whose readers were dropped, and returns how many.
    /// The session's [`Reclaim`](crate::Phase::Reclaim) phase runs it.
    pub(crate) fn reap_taps(&mut self) -> usize {
        let reaped = self.editor.reap_taps();
        if reaped > 0 {
            let open = &self.editor.spec().topology.taps;
            self.output_taps.retain(|id, _| open.contains_key(id));
            self.edited = true;
        }
        reaped
    }

    /// Opens a tap on one event output of a node and returns its reader: the
    /// MIDI events the port carries, each with the frame it was due on
    /// ([`TapEvent`](tutti_graph::TapEvent)), on the clock of the rate the
    /// graph is prepared at now ([`EventTapReader::rate`]). The tap is
    /// compiled by the next commit and fed by every block that renders after
    /// it; a copy of events, the event sibling of [`open_tap`](Self::open_tap).
    ///
    /// `capacity` is a count of events. A non-note-off is refused once that
    /// many are held, a note-off is kept up to
    /// [`note_off_room`](tutti_graph::note_off_room) of it, and each refusal is
    /// counted ([`EventTapReader::take_lost`]). `align` is as for a plan tap:
    /// [`TapAlign::ToOutput`] delays the port by the compensation an output of
    /// the same node gets, so the frames line up with the audio the graph
    /// outputs; [`TapAlign::Raw`] reads it as it leaves its node. Ramps,
    /// segments, harmony and resync are not copied.
    ///
    /// The port must exist now, edits not yet committed included
    /// ([`MissingSource::Node`] or [`MissingSource::EventPort`] otherwise, and
    /// nothing is opened). One that vanishes later reads no events, counts no
    /// loss and fails no commit. Dropping the reader leaves the tap open until
    /// [`close_event_tap`](Self::close_event_tap) or the session's next
    /// [`Reclaim`](crate::Phase::Reclaim) phase, which closes it.
    ///
    /// **A re-prepare at another rate moves the graph's clock to a new scale,**
    /// so a reader's frames stay on the rate it was opened at only until then;
    /// the caller that re-rates the graph reopens its taps
    /// ([`reopen_event_taps`](Self::reopen_event_taps)).
    ///
    /// # Errors
    ///
    /// [`MissingSource`] for a port that is not in the graph now.
    pub fn open_event_tap(
        &mut self,
        source: EventSource,
        align: TapAlign,
        capacity: usize,
    ) -> Result<EventTapReader, MissingSource> {
        let id = EventTapId(self.next_event_tap);
        let reader = self.editor.open_event_tap(
            id,
            EventOut {
                node: source.node,
                port: source.port,
            },
            align,
            capacity,
        )?;
        self.next_event_tap += 1;
        self.event_taps.insert(id, (source, align, capacity));
        self.edited = true;
        Ok(reader)
    }

    /// Closes an event tap: its op leaves the plan at the next commit and the
    /// ring is freed on this side. Closing a closed tap does nothing.
    pub fn close_event_tap(&mut self, id: EventTapId) {
        self.event_taps.remove(&id);
        self.editor.close_event_tap(id);
        self.edited = true;
    }

    /// Opens every event tap again, at the rate the graph is prepared at now,
    /// and returns the new readers: each replaces the tap of the same
    /// [`id`](EventTapReader::id), so the reader it supersedes is fed no more.
    /// What a caller does after a re-prepare at another rate
    /// ([`set_sample_rate`](Self::set_sample_rate); a device session's restart
    /// re-rates the graph too), so that one reader's frames are always on one
    /// scale. A tap whose node was removed since is not reopened and is
    /// dropped from the graph's list.
    ///
    /// The graph cannot do this by itself: a reader belongs to the thread that
    /// drains it, and the new one has to reach that thread.
    pub fn reopen_event_taps(&mut self) -> Vec<EventTapReader> {
        let open: Vec<(EventTapId, (EventSource, TapAlign, usize))> = self
            .event_taps
            .iter()
            .map(|(&id, &opened)| (id, opened))
            .collect();
        let mut readers = Vec::with_capacity(open.len());
        for (id, (source, align, capacity)) in open {
            let port = EventOut {
                node: source.node,
                port: source.port,
            };
            match self.editor.open_event_tap(id, port, align, capacity) {
                Ok(reader) => readers.push(reader),
                Err(_) => {
                    self.event_taps.remove(&id);
                    self.editor.close_event_tap(id);
                }
            }
        }
        self.edited = true;
        readers
    }

    /// Closes the event taps whose readers were dropped, and returns how many.
    /// The session's [`Reclaim`](crate::Phase::Reclaim) phase runs it.
    pub(crate) fn reap_event_taps(&mut self) -> usize {
        let reaped = self.editor.reap_event_taps();
        if reaped > 0 {
            let open = &self.editor.spec().topology.event_taps;
            self.event_taps.retain(|id, _| open.contains_key(id));
            self.edited = true;
        }
        reaped
    }

    /// Points each output tap's lanes at the outputs as they are now.
    fn follow_outputs(&mut self) {
        if self.output_taps.is_empty() {
            return;
        }
        let spec = self.editor.spec_mut();
        let outputs = spec.topology.outputs.clone();
        for (id, &width) in &self.output_taps {
            if let Some(tap) = spec.topology.taps.get_mut(id) {
                // A lane whose output channel is gone (the root narrowed) reads
                // silence, so the reader keeps the lanes it was opened with.
                tap.sources = outputs
                    .iter()
                    .copied()
                    .chain(std::iter::repeat(Source::Zero))
                    .take(width)
                    .collect();
            }
        }
    }

    // --- Meters ---

    /// Declares a level meter on `sources`, one channel each, any node
    /// ports, and returns its reader, **off** until
    /// [`enable`](MeterReader::enable). The meter is compiled by the next
    /// commit and fed by every block that renders after it (a device,
    /// [`Session::render`](crate::Session::render) and
    /// [`Session::render_frame`](crate::Session::render_frame) alike).
    ///
    /// A meter reads before the engine's fold to the device and its declick.
    /// It holds the peak since the last
    /// [`read`](MeterReader::read), so a UI that polls slower than the blocks
    /// still sees a spike; ballistics are the reader's. One reader per meter:
    /// a second consumer opens a second meter.
    ///
    /// `align` is the tap's choice: [`TapAlign::Raw`] reads each source as it
    /// leaves its node (a track's level), [`TapAlign::ToOutput`] delays each
    /// lane by the compensation an output of the same source gets (a ring per
    /// lane), so it lines up with what the outputs carry.
    ///
    /// Every source must exist now, edits not yet committed included
    /// ([`MissingSource`] otherwise, and nothing is opened). Removing a node a
    /// meter reads afterwards leaves that channel on silence, and so does
    /// replacing it with a node that has fewer outputs (the lanes past the
    /// new width); the commit succeeds either way. Dropping the reader
    /// leaves the meter open until [`close_meter`](Self::close_meter) or the
    /// session's next [`Reclaim`](crate::Phase::Reclaim) phase, which closes
    /// it and frees its cells.
    ///
    /// # Errors
    ///
    /// [`MissingSource`] for the first source that is not in the graph now.
    pub fn open_meter(
        &mut self,
        sources: &[GraphSource],
        align: TapAlign,
    ) -> Result<MeterReader, MissingSource> {
        let sources = sources.iter().copied().map(Self::lower).collect();
        self.declare_meter(MeterSpec::Ports { sources, align })
    }

    /// A raw meter on every output port of `node`: a track's level. Its width is
    /// the node's output count now; a later re-shape of the node does not
    /// widen it. A key no node has is [`MissingSource::Node`], not a
    /// zero-width meter.
    ///
    /// # Errors
    ///
    /// [`MissingSource`] for the first source that is not in the graph now.
    pub fn open_node_meter(&mut self, node: NodeKey) -> Result<MeterReader, MissingSource> {
        if self.shape(node).is_none() {
            return Err(MissingSource::Node { node });
        }
        let sources: Vec<GraphSource> = (0..self.node_outputs(node))
            .map(|port| GraphSource::Node(node, port))
            .collect();
        self.open_meter(&sources, TapAlign::Raw)
    }

    fn declare_meter(&mut self, spec: MeterSpec) -> Result<MeterReader, MissingSource> {
        let id = MeterId(self.next_meter);
        let reader = self.editor.open_meter(id, spec)?;
        self.next_meter += 1;
        self.edited = true;
        Ok(reader)
    }

    /// Closes a meter: its op leaves the plan at the next commit and its
    /// cells are freed on this side. Closing a closed meter does nothing.
    pub fn close_meter(&mut self, id: MeterId) {
        self.editor.close_meter(id);
        self.edited = true;
    }

    /// Closes the meters whose readers were dropped, and returns how many.
    /// The session's [`Reclaim`](crate::Phase::Reclaim) phase runs it.
    pub(crate) fn reap_meters(&mut self) -> usize {
        let reaped = self.editor.reap_meters();
        if reaped > 0 {
            self.edited = true;
        }
        reaped
    }

    /// The master meter: the level of each global output channel, before
    /// the engine's fold and declick. Declared and **on** when the graph is
    /// built, so a host that reads it needs to ask for nothing; it follows
    /// the outputs as they are re-wired or widened (up to
    /// [`MAX_ROOT_CHANNELS`](tutti_graph::MAX_ROOT_CHANNELS)). `None` once a
    /// host has taken it ([`take_master_meter`](Self::take_master_meter)). A
    /// block the engine refuses runs no op and does not touch it.
    pub fn master_meter(&mut self) -> Option<&mut MeterReader> {
        self.master_meter.as_mut()
    }

    /// Hands the master meter's reader to its one consumer, which owns it
    /// from here: dropping it closes the meter at the next
    /// [`Reclaim`](crate::Phase::Reclaim) phase.
    pub fn take_master_meter(&mut self) -> Option<MeterReader> {
        self.master_meter.take()
    }

    // --- Shape ---

    /// The graph's global input count.
    pub fn inputs(&self) -> usize {
        self.editor.spec().topology.inputs.count() as usize
    }

    /// The graph's global output count.
    pub fn outputs(&self) -> usize {
        self.editor.spec().topology.outputs.len()
    }

    pub(crate) fn shape(&self, node: NodeKey) -> Option<tutti_graph::Shape> {
        self.editor.shapes().get(&node).copied()
    }

    /// `node`'s input port count.
    pub fn node_inputs(&self, node: NodeKey) -> usize {
        self.shape(node).map_or(0, |s| s.audio_in.count() as usize)
    }

    /// `node`'s output port count.
    pub fn node_outputs(&self, node: NodeKey) -> usize {
        self.shape(node).map_or(0, |s| s.audio_out.count() as usize)
    }

    /// The latency `node` reports: the shape the editor holds, read when the
    /// node was inserted (prepared at the graph's rate) and moved since only
    /// by a re-prepare or a hosted plugin's latency change.
    pub fn node_latency(&self, node: NodeKey) -> Samples {
        self.shape(node).map_or(Samples(0), |s| s.latency.samples())
    }

    /// The tail `node` reports.
    pub fn node_tail(&self, node: NodeKey) -> Tail {
        self.shape(node).map_or(Tail::None, |s| s.tail)
    }

    /// Widen the global outputs to `channels`. New channels read silence.
    /// Never narrows.
    ///
    /// Two requests are refused, and the refusal is returned and kept
    /// ([`Session::root_topology_status`](crate::Session::root_topology_status)):
    ///
    /// - on a root whose topology was declared
    ///   ([`Session::declare_root_topology`](crate::Session::declare_root_topology)),
    ///   anything wider than the declaration: the root is not widened under
    ///   it ([`FoldError::RootWidth`](tutti_types::FoldError::RootWidth));
    /// - past [`MAX_ROOT_CHANNELS`](tutti_graph::MAX_ROOT_CHANNELS): the root
    ///   is widened to that and the excess is not rendered
    ///   ([`FoldError::Unplaced`](tutti_types::FoldError::Unplaced), the first
    ///   channel with no place).
    ///
    /// A request for as many or fewer channels than the root has is a no-op.
    ///
    /// # Errors
    ///
    /// As above.
    pub fn widen_outputs(&mut self, channels: usize) -> Result<(), tutti_types::FoldError> {
        let refused = if let Some(root) = self
            .root
            .as_ref()
            .filter(|r| channels > r.positions().len())
        {
            Some(tutti_types::FoldError::RootWidth {
                declared: root.layout(),
                // Only the report names the request; one past `u16::MAX` is
                // refused either way (it is past `MAX_ROOT_CHANNELS`), so the
                // saturated figure loses nothing.
                requested: ChannelLayout::clamped(channels),
            })
        } else if channels > tutti_graph::MAX_ROOT_CHANNELS {
            Some(tutti_types::FoldError::Unplaced {
                channel: tutti_graph::MAX_ROOT_CHANNELS,
                speaker: tutti_types::Speaker::Discrete,
            })
        } else {
            None
        };
        self.widen_refusal = refused;
        // A declared root is not widened at all when refused; an over-wide
        // request widens as far as the engine renders.
        let target = match refused {
            Some(tutti_types::FoldError::RootWidth { .. }) => 0,
            _ => channels.min(tutti_graph::MAX_ROOT_CHANNELS),
        };
        let outputs = &mut self.editor.spec_mut().topology.outputs;
        if outputs.len() < target {
            outputs.resize(target, Source::Zero);
            self.edited = true;
        }
        refused.map_or(Ok(()), Err)
    }

    /// The last widening [`widen_outputs`](Self::widen_outputs) refused, until
    /// a request is not or a declaration is made.
    pub(crate) fn widen_refusal(&self) -> Option<tutti_types::FoldError> {
        self.widen_refusal
    }

    /// The topology the root was declared with, if one was.
    pub fn root_topology(&self) -> Option<&tutti_types::ChannelTopology> {
        self.root.as_ref()
    }

    /// Declares what the root's channels are, and sizes the root to it: the
    /// root becomes exactly as wide as `root` (widened, or narrowed, its
    /// channels past the new width dropped), since a declaration states the
    /// root and the device is no floor under it. `None` takes it back and leaves
    /// the root at its width.
    ///
    /// `device` is the width the engine renders into, when there is one: a
    /// declaration it cannot place is refused before anything changes. The
    /// engine's folds are republished from the declaration, so the next block
    /// is placed by it.
    ///
    /// # Errors
    ///
    /// The [`FoldError`](tutti_types::FoldError) when the declaration is empty, wider than the engine
    /// renders ([`MAX_ROOT_CHANNELS`](tutti_graph::MAX_ROOT_CHANNELS),
    /// [`FoldError::Unplaced`](tutti_types::FoldError::Unplaced)), or cannot be placed on `device`. Nothing is
    /// changed.
    /// Whether `root` can be a root, and be placed on `device` when there is
    /// one: not empty, no wider than the engine renders, and every channel
    /// has a place on the device ([`Fold::declared`](tutti_types::Fold::declared)).
    pub(crate) fn check_root(
        root: &tutti_types::ChannelTopology,
        device: impl Into<Option<ChannelLayout>>,
    ) -> Result<(), tutti_types::FoldError> {
        use tutti_types::{Fold, FoldEnd, FoldError};
        let width = root.positions().len();
        if width == 0 {
            return Err(FoldError::Empty(FoldEnd::Source));
        }
        if width > tutti_graph::MAX_ROOT_CHANNELS {
            return Err(FoldError::Unplaced {
                channel: tutti_graph::MAX_ROOT_CHANNELS,
                speaker: root.positions()[tutti_graph::MAX_ROOT_CHANNELS],
            });
        }
        if let Some(device) = device.into() {
            Fold::declared(root, device)?;
        }
        Ok(())
    }

    pub(crate) fn declare_root(
        &mut self,
        root: Option<tutti_types::ChannelTopology>,
        device: Option<ChannelLayout>,
    ) -> Result<(), tutti_types::FoldError> {
        use tutti_types::Fold;
        let width = match &root {
            Some(root) => {
                Self::check_root(root, device)?;
                root.positions().len()
            }
            None => {
                if let Some(device) = device {
                    let outputs = self.outputs();
                    let width = ChannelLayout::try_from(outputs)
                        .map_err(|_| tutti_types::FoldError::TooWide(outputs))?;
                    Fold::widths(width, device)?;
                }
                self.outputs()
            }
        };
        // The engine first: it checks the device again, and a refusal there
        // leaves the graph as it was.
        if let (Some(output), Some(device)) = (&self.output, device) {
            output.declare_root(root.clone(), device)?;
        }
        self.root = root;
        self.widen_refusal = None;
        let outputs = &mut self.editor.spec_mut().topology.outputs;
        if outputs.len() != width {
            outputs.resize(width, Source::Zero);
            self.edited = true;
            self.follow_outputs();
        }
        Ok(())
    }

    // --- Latency ---

    /// What compensation this graph needs, folded over the graph as edited
    /// (committed or not): audio edges plus each node's event and
    /// param-modulation sources. Mutates nothing.
    pub fn latency_plan(&self) -> Compensation {
        tutti_graph::latency_plan(self.editor.spec())
    }

    /// The compensation the next commit's plan carries: the spec compiled
    /// against the shapes, as the commit will compile it. Nothing is
    /// inserted — PDC is the compiler's. `None` when the spec does not
    /// compile (the commit will say why).
    pub fn planned_compensation(&self) -> Option<Compensation> {
        let valid = self.editor.spec().validate().ok()?;
        let (plan, _) = tutti_graph::compile(
            &valid,
            self.editor.shapes(),
            self.editor.prepare(),
            self.editor.base().map(|p| &**p),
        )
        .ok()?;
        Some(compensation_of(&plan))
    }

    /// The plan last sent to the audio thread (`Editor::base`).
    pub fn sent_plan(&self) -> Option<Arc<tutti_graph::Plan>> {
        self.editor.base().cloned()
    }

    /// How many commits have sent a plan.
    pub fn commits_sent(&self) -> u64 {
        self.commits
    }

    /// Whether an edit waits for a commit: what the session's next update
    /// commits on.
    pub fn is_edited(&self) -> bool {
        self.edited
    }

    /// The compensation of the plan sent last: what the session publishes,
    /// since it is what the executor is handed.
    pub fn sent_compensation(&self) -> Option<Compensation> {
        self.editor.base().map(|plan| compensation_of(plan))
    }

    /// A node's latency moved at runtime (a plugin's latency cell): hand
    /// `latency` — the figure the node's `Shape` declares now, which for a
    /// plugin is `PluginControls::declared_latency` — to the editor
    /// (`Editor::set_latency`), clamped to what PDC compensates, so the next
    /// commit moves PDC to it without touching the running node.
    ///
    /// Returns whether the editor's figure moved.
    #[cfg(feature = "plugin")]
    pub fn refresh_node_latency(&mut self, node: NodeKey, latency: Latency) -> bool {
        if !self.nodes.contains_key(&node) {
            return false;
        }
        // Clamped to what PDC compensates, as the editor clamps a latency it
        // probes at insert: `set_latency` refuses a figure past it, and a
        // refused one would never be retried (the poll only fires again when
        // the plugin's figure moves).
        let latency = clamp_latency(latency);
        if self.node_latency(node) == latency.samples() {
            return false;
        }
        match self.editor.set_latency(node, latency) {
            Ok(()) => {
                self.edited = true;
                true
            }
            Err(e) => {
                tracing::error!("graph: latency of {node:?} refused: {e}");
                false
            }
        }
    }

    // --- Committing and rendering ---

    /// Drain what the executor sent back, freeing retired units here: on
    /// the main thread (`crate::main_thread`).
    pub(crate) fn collect(&mut self) {
        crate::main_thread();
        self.editor.collect();
    }

    /// The output ports that lost events since the last call
    /// (`Editor::take_event_losses`).
    pub(crate) fn take_event_losses(
        &mut self,
    ) -> impl Iterator<Item = tutti_graph::EventLoss> + '_ {
        self.editor.take_event_losses()
    }

    /// Refusals no loss entry named (`Editor::take_unreported_event_losses`).
    pub(crate) fn take_unreported_event_losses(&mut self) -> u64 {
        self.editor.take_unreported_event_losses()
    }

    /// `Editor::commit`. `Sent` clears the edit flag; `Retry` keeps it.
    pub(crate) fn commit(&mut self) -> Committed {
        match self.editor.commit() {
            Ok(()) => {
                self.edited = false;
                self.commits += 1;
                self.pump_local();
                Committed::Sent
            }
            Err(CommitError::Backpressure | CommitError::Repreparing) => Committed::Retry,
            Err(e) => {
                // Not retried: the same spec fails the same way next update.
                // The next edit marks the graph edited again.
                tracing::error!("graph: commit refused: {e}");
                Committed::Refused
            }
        }
    }

    /// Render one frame on the local executor, under `transport`.
    pub(crate) fn render_local(&mut self, transport: &Transport, output: &mut [f32]) {
        let local = self
            .local
            .as_mut()
            .expect("render_frame needs the audio side, and it was taken");
        render(&mut local.exec, &mut local.scratch, transport, &[], output);
    }
}

/// Render one frame of `exec` into `output` (one sample per global output),
/// reading `input` (one sample per global input).
///
/// `output` wider than the plan reads silence past it; narrower, the extra
/// channels are dropped — the width a caller passes is its own business.
fn render(
    exec: &mut Executor,
    scratch: &mut Vec<Vec<f32>>,
    transport: &Transport,
    input: &[f32],
    output: &mut [f32],
) {
    exec.apply_pending();
    let Some(plan) = exec.plan() else {
        output.fill(0.0);
        return;
    };
    let (ins, outs) = (plan.global_inputs() as usize, plan.global_outputs());
    scratch.resize_with(outs, || vec![0.0]);
    let inputs: Vec<&[f32]> = (0..ins)
        .map(|c| input.get(c).map_or(&[0.0f32][..], std::slice::from_ref))
        .collect();
    let mut outputs: Vec<&mut [f32]> = scratch.iter_mut().map(|c| &mut c[..1]).collect();
    exec.process(1, transport, &inputs, &mut outputs);
    for (c, o) in output.iter_mut().enumerate() {
        *o = scratch.get(c).map_or(0.0, |s| s[0]);
    }
}

/// What an export renders: the whole graph, or one node's sub-graph.
#[cfg(feature = "export")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AudioNodeTarget {
    /// The master: every global output.
    Master,
    /// `node`'s outputs.
    Node(NodeKey),
}

/// Resolves a host's modulation keys to graph nodes.
///
/// `tutti_modulation`'s declarations key a target by `TargetKey { node: u64, param }`
/// and a source by `SourceKey(u64)`; the `u64` is the host's own minting (a
/// Bevy entity's bits, say), and this is how the session's
/// [`Modulation`](crate::Phase::Modulation) phase reads it ([`ModInput`](crate::ModInput)).
#[cfg(feature = "modulation")]
pub trait ModKeys {
    /// The node a target's key names.
    fn node(&self, target: u64) -> Option<NodeKey>;
    /// What feeds source `source`'s node.
    fn feed(&self, source: tutti_modulation::SourceKey) -> Option<tutti_nodes::Feed>;
}

/// `tutti_nodes::ModHost` over the graph's nodes and the host's keys: a
/// target's key names its node, a native target's knob is that node's
/// `Param` cell, a plugin target feeds the node's event input 0 and starts at
/// the plugin's mirrored value (the session's record of it,
/// `HostedPlugin::param_values`).
#[cfg(feature = "modulation")]
struct ModHostOver<'a> {
    nodes: &'a BTreeMap<NodeKey, Entry>,
    #[cfg(feature = "plugin")]
    plugins: &'a BTreeMap<NodeKey, crate::HostedPlugin>,
    keys: &'a dyn ModKeys,
    /// Each plugin target the apply resolved, by node and parameter: where
    /// `Phase::Plugins` writes a GUI edit (`ModState::plugin_target`).
    plugin_targets: std::cell::RefCell<PluginTargets>,
}

/// A plugin parameter's modulation target, by its node and parameter id.
#[cfg(feature = "modulation")]
pub(crate) type PluginTargets = BTreeMap<(NodeKey, u32), tutti_modulation::TargetKey>;

#[cfg(feature = "modulation")]
impl tutti_nodes::ModHost for ModHostOver<'_> {
    fn target(&self, target: tutti_modulation::TargetKey) -> Option<tutti_nodes::Target> {
        let node = self.keys.node(target.node)?;
        Some(match target.param.as_unit() {
            // A native param, at any index: the node's own cell for that
            // address is the lane's knob.
            Some(addr) => tutti_nodes::Target::Native {
                node,
                knob: self
                    .nodes
                    .get(&node)
                    .and_then(|e| e.params.as_ref())
                    .and_then(|p| p.cell(addr)),
            },
            // A plugin target waits for its mirror: a lane built without
            // a knob could not take one later.
            None => {
                let tutti_types::ParamAddr::Id(id) = target.param else {
                    return None;
                };
                // A node the session hosts no plugin at has no mirror: its
                // lane has no knob to start from.
                #[cfg(feature = "plugin")]
                let knob = self
                    .plugins
                    .get(&node)
                    .map_or(Some(None), |p| p.param_value(id))?;
                #[cfg(not(feature = "plugin"))]
                let knob = None;
                self.plugin_targets.borrow_mut().insert((node, id), target);
                tutti_nodes::Target::Foreign {
                    node,
                    port: 0,
                    knob,
                }
            }
        })
    }

    fn feed(&self, source: tutti_modulation::SourceKey) -> Option<tutti_nodes::Feed> {
        self.keys.feed(source)
    }
}

/// The audio thread's half of a graph, for a test that renders what a device
/// would hear: every commit lands on it, and rendering it plays the graph.
/// Taken with [`Graph::take_audio_side`].
pub struct AudioSide {
    exec: Executor,
    transport: Transport,
    scratch: Vec<Vec<f32>>,
    /// A fork's own editor, kept for as long as its executor runs: the
    /// executor sends its boxes back to it. `None` for the live graph's
    /// audio side, whose editor stays in the graph.
    _editor: Option<Editor>,
}

impl std::fmt::Debug for AudioSide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioSide")
            .field("exec", &self.exec)
            .field("transport", &self.transport)
            .field("_editor", &self._editor)
            .finish_non_exhaustive()
    }
}

impl AudioSide {
    fn live(exec: Executor) -> Self {
        Self {
            exec,
            transport: Transport::default(),
            scratch: Vec::new(),
            _editor: None,
        }
    }

    /// A fork's pair, rendered as an audio side.
    fn forked(editor: Editor, exec: Executor) -> Self {
        Self {
            exec,
            transport: Transport::default(),
            scratch: Vec::new(),
            _editor: Some(editor),
        }
    }

    /// Renders one frame: `input` one sample per global input, `output` one
    /// per global output. Commits sent since the last call land first.
    pub fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        render(
            &mut self.exec,
            &mut self.scratch,
            &self.transport,
            input,
            output,
        );
    }

    /// The transport every block from here on renders under, as a device's
    /// engine hands its graph the playhead: a node that reads the transport
    /// from its `Env` (a placed sampler voice) plays on it. Stopped at beat
    /// zero until set.
    pub fn set_transport(&mut self, transport: Transport) {
        self.transport = transport;
    }

    /// Renders `frames` frames with no global input into `output`, planar,
    /// one slice per global output, in the blocks a device would hand over:
    /// executor blocks of up to `block` frames, each under the transport
    /// last [set](Self::set_transport) (stopped at beat zero until one is).
    ///
    /// # Panics
    ///
    /// If `block` is zero, or past the graph's prepared maximum.
    pub fn render(&mut self, frames: usize, block: usize, output: &mut [Vec<f32>]) {
        assert!(block > 0, "a zero-frame block");
        for o in output.iter_mut() {
            o.clear();
            o.resize(frames, 0.0);
        }
        let exec = &mut self.exec;
        let mut done = 0;
        while done < frames {
            let len = (frames - done).min(block);
            exec.apply_pending();
            let (ins, outs) = exec
                .plan()
                .map_or((0, 0), |p| (p.global_inputs() as usize, p.global_outputs()));
            let zeros = vec![0.0f32; len];
            let inputs: Vec<&[f32]> = (0..ins).map(|_| &zeros[..]).collect();
            let mut planes: Vec<Vec<f32>> = vec![vec![0.0; len]; outs];
            let mut slices: Vec<&mut [f32]> = planes.iter_mut().map(|p| &mut p[..]).collect();
            if exec.plan().is_some() {
                exec.process(len, &self.transport, &inputs, &mut slices);
            }
            for (o, p) in output.iter_mut().zip(&planes) {
                o[done..done + len].copy_from_slice(p);
            }
            done += len;
        }
    }
}

/// An export's fork, gathered on the main thread
/// ([`Graph::gather_for_export`]) and not yet built: `Send`, holding nothing
/// of the live graph.
#[cfg(feature = "export")]
pub(crate) struct PendingExport {
    fork: tutti_graph::PendingFork,
    pool: Option<Arc<dyn tutti_graph::Pool>>,
    /// The live root's declared topology, for an export of the master: the
    /// file is placed from it, as the device is.
    root: Option<tutti_types::ChannelTopology>,
}

#[cfg(feature = "export")]
impl PendingExport {
    /// Build the fork (on a worker: a hosted plugin's `plugin-server` launch
    /// is here) into the graph the export renders.
    ///
    /// # Errors
    ///
    /// A deferred fork source's failure, or a fork that does not commit,
    /// with what was built, to drop on the main thread.
    pub(crate) fn build(
        self,
    ) -> Result<tutti_export::RenderGraph, (tutti_export::Error, tutti_graph::ForkRemains)> {
        let mut graph = tutti_export::RenderGraph::build(self.fork)?;
        graph.set_pool(self.pool);
        graph.set_root_topology(self.root);
        Ok(graph)
    }
}
