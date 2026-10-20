//! [`Graph`]: a `tutti-graph` [`Editor`], the nodes inserted through it, the
//! event feeds into them, the crossfades waiting on a re-prepare, and — until
//! an engine or a test takes it — the [`Executor`] it sends to.
//!
//! What each method does to the editor:
//!
//! | `Graph` | the editor |
//! |---|---|
//! | `insert` | `Editor::insert` of the node's own [`IntoNode`] at a [`NodeKey`] minted by `AudioNode::fresh`, its controls handed back |
//! | `set_source`, `set_output_source`, `widen_outputs` | written into `editor.spec_mut()` |
//! | `set_param` | the node's [`ParamSet`], when it has one |
//! | `replace`, `crossfade` | `Editor::replace_or_swap` with a [`Fade`]: a plain swap when there is nothing to fade from |
//! | `node_latency`, `node_tail`, `node_inputs`, `node_outputs` | the editor's [`Shapes`](tutti_graph::Shapes) |
//! | `latency_plan` | `tutti_types::latency::plan` over the whole spec: audio edges plus event and param-modulation sources |
//! | `set_feed`, `move_feeds` | the union of every feeder's sources, written as a sink's event edges |
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

use tutti_core::{AudioNode, Compensation, CrossfadeCurve, Samples, Tail};
use tutti_graph::{
    CommitError, Editor, EventEdge, EventIn, EventOut, Executor, Fade, GraphInvalid, IntoNode,
    ParamIn, ParamMod, ParamRange, ParamSet, Prepare, Transport,
};
use tutti_types::graph::{Edge, InPort, NodeKey, OutPort, Source};
#[cfg(feature = "plugin")]
use tutti_types::Latency;
use tutti_types::{ChannelLayout, SampleRate, Seconds, UnitParam};

use crate::SessionNode;

/// The largest block a device's graph is prepared for.
///
/// A device block longer than this is rendered by `tutti_core::Engine` as
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
/// [`AudioNode`]. Read back with [`Graph::source`] and
/// [`Graph::output_source`]; written with [`Graph::set_source`] and
/// [`Graph::set_output_source`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum GraphSource {
    /// Output `port` of `node`.
    Node(AudioNode, usize),
    /// Global graph input `port` — a hardware or host input channel.
    Input(usize),
    /// Silence: what a port nothing has written holds.
    #[default]
    Silence,
}

impl GraphSource {
    /// Output `port` of `node`.
    pub const fn node(node: AudioNode, port: usize) -> Self {
        Self::Node(node, port)
    }
}

/// One event output: `node`'s event output `port`. From an [`AudioNode`]
/// alone, its output 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EventSource {
    /// The node.
    pub node: AudioNode,
    /// Its event output.
    pub port: u16,
}

impl EventSource {
    /// Names `node`'s event output `port`.
    pub const fn new(node: AudioNode, port: u16) -> Self {
        Self { node, port }
    }

    /// A total order for a set of sources: by node, then port.
    fn key(&self) -> (u64, u16) {
        (self.node.key().0, self.port)
    }
}

impl From<AudioNode> for EventSource {
    fn from(node: AudioNode) -> Self {
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
    Failed(String),
}

impl<U> std::fmt::Debug for ReplaceRefused<U> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy(_) => f.write_str("Busy(..)"),
            Self::Failed(why) => f.debug_tuple("Failed").field(why).finish(),
        }
    }
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

/// Why [`Graph::crossfade`] took nothing: the graph is poisoned, or the node
/// is not in it. The incoming node was dropped.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("crossfade refused: {0}")]
pub struct CrossfadeRefused(pub String);

/// One node's handle, and its params by address when it has any.
struct Entry {
    node: AudioNode,
    /// What [`set_param`](Graph::set_param) writes through, and what a fork
    /// of the node starts from.
    params: Option<ParamSet>,
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
        node: AudioNode,
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
        node: AudioNode,
        fade: Seconds,
        curve: CrossfadeCurve,
    ) -> Result<Box<dyn Any + Send>, ReplaceRefused<Box<dyn ErasedSwap>>> {
        match graph.replace(node, self.0, fade, curve) {
            Ok(controls) => {
                graph.set_node_params(node, N::params(&controls));
                Ok(Box::new(controls))
            }
            Err(ReplaceRefused::Busy(n)) => Err(ReplaceRefused::Busy(Box::new(Swap(n)))),
            Err(ReplaceRefused::Failed(why)) => Err(ReplaceRefused::Failed(why)),
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
        node: AudioNode,
        fade: Seconds,
        curve: CrossfadeCurve,
    ) -> Result<Box<dyn Any + Send>, ReplaceRefused<Box<dyn ErasedSwap>>> {
        match graph.replace_plugin(node, self.0, fade, curve) {
            Ok(()) => Ok(Box::new(())),
            Err(ReplaceRefused::Busy(c)) => Err(ReplaceRefused::Busy(Box::new(PluginSwap(c)))),
            Err(ReplaceRefused::Failed(why)) => Err(ReplaceRefused::Failed(why)),
        }
    }
}

/// A crossfade parked while the graph re-prepares.
struct Parked {
    id: CrossfadeId,
    node: AudioNode,
    swap: Box<dyn ErasedSwap>,
    fade: Seconds,
    curve: CrossfadeCurve,
}

/// What a parked crossfade came to on a retry.
pub(crate) enum Retried {
    Landed {
        node: AudioNode,
        id: CrossfadeId,
        controls: Box<dyn Any + Send>,
    },
    Failed {
        node: AudioNode,
        id: CrossfadeId,
        why: String,
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
/// replace a node under a fade) and takes an [`AudioNode`] and a
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
    /// Each sink's event input 0, by feeder: the sources each feeder feeds
    /// it. Written into the spec as their union.
    feeds: BTreeMap<NodeKey, BTreeMap<&'static str, Vec<EventSource>>>,
    /// Crossfades refused `Busy`, in request order.
    parked: VecDeque<Parked>,
    next_crossfade: u64,
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

/// `node`'s key. [`AudioNode::fresh`] draws it from [`NodeKey::fresh`]'s
/// process-wide counter, so a key minted this way is unique without a map.
pub(crate) fn key(node: AudioNode) -> NodeKey {
    node.0
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
    Latency::new(
        latency
            .samples()
            .min(tutti_types::latency::MAX_NODE_LATENCY),
    )
}

impl Graph {
    /// An empty graph with `inputs` global inputs and `outputs` global
    /// outputs, prepared for `rate`, `max_block` and the device's callback
    /// `quantum` (when known), its executor local.
    pub(crate) fn new(
        inputs: usize,
        outputs: usize,
        rate: SampleRate,
        max_block: Samples,
        quantum: Option<Samples>,
    ) -> Self {
        let (mut editor, exec) = Editor::new(prepare_for(rate, max_block, quantum));
        let topology = &mut editor.spec_mut().topology;
        topology.inputs = ChannelLayout::from_count(inputs as u16);
        topology.outputs = vec![Source::Zero; outputs];
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
            feeds: BTreeMap::new(),
            parked: VecDeque::new(),
            next_crossfade: 0,
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

    /// A fresh pool of the configured size, its threads named `name`, or
    /// `None` for one participant (render on the caller alone).
    pub(crate) fn render_pool(&self, name: &str) -> Option<Arc<dyn tutti_graph::Pool>> {
        let n = match self.render_workers {
            0 => std::thread::available_parallelism().map_or(1, |n| n.get()),
            n => n,
        };
        (n > 1).then(|| {
            Arc::new(tutti_core::WorkerPool::builder(n).name(name).build())
                as Arc<dyn tutti_graph::Pool>
        })
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
        transport: &tutti_core::Transport,
    ) -> Result<tutti_core::Engine, tutti_core::GraphEngineError> {
        let mut exec = self.take_executor();
        exec.set_pool(self.render_pool("tutti-worker"));
        tutti_core::Engine::new(transport, &mut self.editor, exec)
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
        if let Some(cause) = self.editor.poisoned() {
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
    /// [`add`](Self::add), which does.
    pub fn insert<N: IntoNode>(&mut self, node: N) -> (AudioNode, N::Controls) {
        let id = AudioNode::fresh();
        let controls = self.editor.insert(key(id), N::kind(), node);
        self.nodes.insert(
            key(id),
            Entry {
                node: id,
                params: None,
            },
        );
        self.edited = true;
        (id, controls)
    }

    /// [`insert`](Self::insert) a [`SessionNode`], its params addressed by
    /// [`SessionNode::params`].
    pub fn add<N: SessionNode>(&mut self, node: N) -> (AudioNode, N::Controls) {
        let (id, controls) = self.insert(node);
        self.set_node_params(id, N::params(&controls));
        (id, controls)
    }

    /// Inserts a hosted plugin: bound (`PluginClient::bind`, its typestate
    /// transition to a graph node) and inserted as the `Node` it then is,
    /// which hands the editor its own fork source (a fork by state transfer)
    /// and declares its latency in its `Shape`. Capture its controls first:
    /// the plugin is out of reach once inserted.
    #[cfg(feature = "plugin")]
    pub fn insert_plugin(&mut self, client: Box<tutti_plugin::handles::PluginClient>) -> AudioNode {
        let node = AudioNode::fresh();
        let _controls = self.editor.insert(key(node), PLUGIN_KIND, client.bind());
        self.nodes.insert(key(node), Entry { node, params: None });
        self.edited = true;
        node
    }

    /// Swap the plugin at `node` for `client`, as [`replace`](Self::replace)
    /// swaps any node: crossfading when the running node's shape fits the
    /// bound plugin's, else a plain swap at the same key. Refused, handing
    /// `client` back unbound, while a re-prepare is between its two commits.
    #[cfg(feature = "plugin")]
    pub fn replace_plugin(
        &mut self,
        node: AudioNode,
        client: Box<tutti_plugin::handles::PluginClient>,
        fade: Seconds,
        curve: CrossfadeCurve,
    ) -> Result<(), ReplaceRefused<Box<tutti_plugin::handles::PluginClient>>> {
        // Asked here, before binding: `replace` hands back what it refuses,
        // and a bound plugin cannot be unbound.
        if let Some(cause) = self.editor.poisoned() {
            return Err(ReplaceRefused::Failed(format!(
                "the graph is poisoned ({cause}); build a new one"
            )));
        }
        if self.editor.is_repreparing() {
            return Err(ReplaceRefused::Busy(client));
        }
        match self.replace(node, client.bind(), fade, curve) {
            Ok(_controls) => Ok(()),
            Err(ReplaceRefused::Failed(why)) => Err(ReplaceRefused::Failed(why)),
            Err(ReplaceRefused::Busy(_)) => unreachable!("checked above"),
        }
    }

    /// The control-thread half of a copy of `target` for an offline render
    /// at `rate`, sharing no state with this graph: `Editor::fork_gather`
    /// with `ForkMode::Offline(ctx)`, through tutti-export
    /// (`RenderGraph::gather`). [`PendingExport::build`] makes the rest on a
    /// worker.
    ///
    /// # Errors
    ///
    /// What `RenderGraph::gather` refuses (a node with no outputs or not in
    /// the graph, a node that cannot be forked).
    #[cfg(feature = "export")]
    pub fn gather_for_export(
        &self,
        target: AudioNodeTarget,
        ctx: &tutti_core::transport::OfflineTransport,
        rate: SampleRate,
    ) -> tutti_export::Result<PendingExport> {
        let target = match target {
            AudioNodeTarget::Master => tutti_graph::ForkTarget::Master,
            AudioNodeTarget::Node(node) => tutti_graph::ForkTarget::Node(key(node)),
        };
        let fork = tutti_export::RenderGraph::gather(
            &self.editor,
            target,
            tutti_graph::ForkMode::Offline(ctx),
            rate,
        )?;
        Ok(PendingExport {
            fork,
            // Its own pool, not the live engine's: the live callback would
            // find a shared one busy with the export's job and render alone.
            pool: self.render_pool("tutti-export"),
        })
    }

    /// Addresses `node`'s params by `params` (a `ParamNode`'s controls): what
    /// [`set_param`](Self::set_param) writes through for it, and what a fork
    /// of it starts from. `None` takes the address away.
    pub fn set_node_params(&mut self, node: AudioNode, params: Option<ParamSet>) {
        if let Some(entry) = self.nodes.get_mut(&key(node)) {
            entry.params = params;
        }
    }

    /// Swaps the unit behind `node` for `incoming` under a `fade` along
    /// `curve` when its shape fits the running unit's, else as a plain swap
    /// on the next commit (`Editor::replace_or_swap`). Hands back its
    /// controls; the caller addresses its params
    /// ([`set_node_params`](Self::set_node_params)). Refused as
    /// [`ReplaceRefused::Busy`] with the node handed back while the graph
    /// re-prepares, and for good on a poisoned graph.
    /// [`crossfade`](Self::crossfade) waits out a re-prepare instead.
    pub fn replace<N: IntoNode>(
        &mut self,
        node: AudioNode,
        incoming: N,
        fade: Seconds,
        curve: CrossfadeCurve,
    ) -> Result<N::Controls, ReplaceRefused<N>> {
        if let Some(cause) = self.editor.poisoned() {
            return Err(ReplaceRefused::Failed(format!(
                "the graph is poisoned ({cause}); build a new one"
            )));
        }
        if self.editor.is_repreparing() {
            return Err(ReplaceRefused::Busy(incoming));
        }
        let k = key(node);
        if !self.nodes.contains_key(&k) {
            return Err(ReplaceRefused::Failed(format!(
                "{node:?} is not in the graph"
            )));
        }
        let rate = self.editor.prepare().sample_rate();
        let controls = self
            .editor
            .replace_or_swap(k, incoming, Fade::seconds(fade, rate, curve))
            .map_err(|e| ReplaceRefused::Failed(e.to_string()))?;
        if let Some(entry) = self.nodes.get_mut(&k) {
            entry.params = None;
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
        node: AudioNode,
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
                self.set_node_params(node, N::params(&controls));
                Ok(Crossfade::Landed(controls))
            }
            Err(ReplaceRefused::Busy(n)) => Ok(Crossfade::Waiting(self.park(
                node,
                Box::new(Swap(n)),
                fade,
                curve,
            ))),
            Err(ReplaceRefused::Failed(why)) => Err(CrossfadeRefused(why)),
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
        node: AudioNode,
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
            Err(ReplaceRefused::Failed(why)) => Err(CrossfadeRefused(why)),
        }
    }

    fn park(
        &mut self,
        node: AudioNode,
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
                Err(ReplaceRefused::Failed(why)) => {
                    tracing::error!("graph: crossfade of {node:?} refused: {why}");
                    out.push(Retried::Failed { node, id, why });
                }
            }
        }
        out
    }

    /// How many event inputs `node` declares.
    pub fn node_event_inputs(&self, node: AudioNode) -> usize {
        self.shape(node).map_or(0, |s| usize::from(s.event_in))
    }

    /// Feeds `sink`'s event input `port` from exactly `sources` (fan-in
    /// merges them by offset; the graph orders ties by source). Touches
    /// nothing when that is what it already holds. Event edges from a node
    /// the graph did not insert through [`insert`](Self::insert) (a
    /// modulation lane on a plugin's event input) are not the caller's, and
    /// stay.
    ///
    /// On input 0 of a fed sink, the feeds write over this on their next
    /// change: [`set_feed`](Self::set_feed) is the declared form.
    pub fn set_event_sources(&mut self, sink: AudioNode, port: u16, sources: &[EventSource]) {
        let at = EventIn {
            node: key(sink),
            port,
        };
        let mut want: Vec<EventOut> = sources
            .iter()
            .map(|s| EventOut {
                node: key(s.node),
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
    pub fn event_sources(&self, sink: AudioNode, port: u16) -> Vec<EventSource> {
        let at = EventIn {
            node: key(sink),
            port,
        };
        self.editor
            .spec()
            .events
            .get(&at)
            .into_iter()
            .flatten()
            .map(|e| {
                let from = e.from();
                EventSource::new(AudioNode(from.node), from.port)
            })
            .collect()
    }

    // --- Event feeds ---

    /// Sets what `feeder` feeds `sink`'s event input 0, replacing what it fed
    /// before (empty: nothing). The sink's input reads the union of every
    /// feeder's sources that are in the graph, written when it changes; a
    /// sink with no event input takes nothing, and keeps the feed.
    ///
    /// Each feeder (a clip, a route rule, a declaration) owns only its own
    /// entry, so feeders never overwrite each other.
    pub fn set_feed(&mut self, sink: AudioNode, feeder: &'static str, sources: Vec<EventSource>) {
        if sources.is_empty() {
            // No entry is made for a feeder that feeds nothing.
            let Some(feeds) = self.feeds.get_mut(&key(sink)) else {
                return;
            };
            if feeds.remove(feeder).is_none() {
                return;
            }
            if feeds.is_empty() {
                self.feeds.remove(&key(sink));
            }
        } else {
            let feeds = self.feeds.entry(key(sink)).or_default();
            if feeds.get(feeder) == Some(&sources) {
                return;
            }
            feeds.insert(feeder, sources);
        }
        self.write_feeds(sink);
    }

    /// What `feeder` feeds `sink`: empty when nothing.
    pub fn feed(&self, sink: AudioNode, feeder: &'static str) -> &[EventSource] {
        self.feeds
            .get(&key(sink))
            .and_then(|f| f.get(feeder))
            .map_or(&[], Vec::as_slice)
    }

    /// Every source any feeder feeds `sink`, sorted and without repeats.
    pub fn fed(&self, sink: AudioNode) -> Vec<EventSource> {
        let mut all: Vec<EventSource> = self
            .feeds
            .get(&key(sink))
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
    pub fn move_feeds(&mut self, from: AudioNode, to: AudioNode) {
        if from == to {
            return;
        }
        let Some(moved) = self.feeds.remove(&key(from)) else {
            return;
        };
        let into = self.feeds.entry(key(to)).or_default();
        for (feeder, sources) in moved {
            into.insert(feeder, sources);
        }
        self.write_feeds(from);
        self.write_feeds(to);
    }

    /// Write `sink`'s feeds into its event input 0: the union of every
    /// feeder's sources still in the graph.
    fn write_feeds(&mut self, sink: AudioNode) {
        if !self.nodes.contains_key(&key(sink)) || self.node_event_inputs(sink) == 0 {
            return;
        }
        let union: Vec<EventSource> = self
            .fed(sink)
            .into_iter()
            .filter(|s| self.contains(s.node))
            .collect();
        self.set_event_sources(sink, 0, &union);
    }

    /// Takes `node` out of the graph. Every edge to and from it reads silence
    /// afterwards, so a sink still naming it is left silent rather than
    /// dangling, and the feeds into and from it go.
    ///
    /// Returns whether it was there; removing a node twice is a no-op.
    pub fn remove(&mut self, node: AudioNode) -> bool {
        if self.nodes.remove(&key(node)).is_none() {
            return false;
        }
        self.editor.remove(key(node));
        self.feeds.remove(&key(node));
        for feeds in self.feeds.values_mut() {
            for sources in feeds.values_mut() {
                sources.retain(|s| s.node != node);
            }
            feeds.retain(|_, s| !s.is_empty());
        }
        self.feeds.retain(|_, f| !f.is_empty());
        self.edited = true;
        true
    }

    /// Whether `node` is in the graph, a modulation lane or signal node
    /// included (staged, committed or not).
    pub fn contains(&self, node: AudioNode) -> bool {
        self.editor.spec().topology.nodes.contains_key(&key(node))
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
    pub fn modulate(
        &mut self,
        m: &mut tutti_nodes::Modulation,
        decl: &tutti_mod::Declarations<'_>,
        keys: &dyn ModKeys,
    ) -> Result<tutti_nodes::Applied, tutti_nodes::ReconcileError> {
        let host = ModHostOver {
            nodes: &self.nodes,
            keys,
        };
        let applied = m.apply(decl, &mut self.editor, &host)?;
        if applied.commit {
            self.edited = true;
        }
        Ok(applied)
    }

    /// Schedules the note-envelope resyncs `m` owes, after the commit that
    /// sent their nodes (`Modulation::send_resyncs`).
    #[cfg(feature = "modulation")]
    pub fn send_mod_resyncs(&mut self, m: &mut tutti_nodes::Modulation) {
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
    /// without the param takes nothing.
    pub fn set_param(&mut self, node: AudioNode, param: UnitParam, value: f32) {
        if let Some(params) = self.nodes.get(&key(node)).and_then(|e| e.params.as_ref()) {
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
                node: key(node),
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
    pub fn source(&self, node: AudioNode, port: usize) -> GraphSource {
        let at = InPort {
            node: key(node),
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
    pub fn set_source(&mut self, node: AudioNode, port: usize, source: GraphSource) {
        assert!(
            !matches!(source, GraphSource::Node(from, _) if from == node),
            "a node cannot feed its own input"
        );
        let at = InPort {
            node: key(node),
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
    pub fn declares_param(&self, node: AudioNode, param: UnitParam) -> bool {
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
        node: AudioNode,
        param: UnitParam,
        source: Option<AudioNode>,
        range: ParamRange,
    ) -> Result<(), GraphInvalid> {
        let at = ParamIn {
            node: key(node),
            param,
        };
        if range.min.is_nan() || range.max.is_nan() {
            return Err(GraphInvalid::BadParamRange { at, range });
        }
        let want = source.map(|from| EventOut {
            node: key(from),
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
    pub fn clear_param_mod(&mut self, node: AudioNode, param: UnitParam) {
        let at = ParamIn {
            node: key(node),
            param,
        };
        if self.editor.spec_mut().params.remove(&at).is_some() {
            self.edited = true;
        }
    }

    /// How `node`'s `param` is modulated, as the graph value holds it.
    pub fn param_mod(&self, node: AudioNode, param: UnitParam) -> Option<ParamMod> {
        self.editor
            .spec()
            .params
            .get(&ParamIn {
                node: key(node),
                param,
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
        self.edited = true;
    }

    /// Feeds **every** global output from `node`: channel `c` from its port
    /// `c % node_outputs(node)`, or silence if it has no outputs.
    pub fn set_outputs_from(&mut self, node: AudioNode) {
        let outs = self.node_outputs(node);
        let k = key(node);
        for (c, source) in self
            .editor
            .spec_mut()
            .topology
            .outputs
            .iter_mut()
            .enumerate()
        {
            *source = if outs == 0 {
                Source::Zero
            } else {
                Source::Node(OutPort {
                    node: k,
                    port: (c % outs) as u16,
                })
            };
        }
        self.edited = true;
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

    fn shape(&self, node: AudioNode) -> Option<tutti_graph::Shape> {
        self.editor.shapes().get(&key(node)).copied()
    }

    /// `node`'s input port count.
    pub fn node_inputs(&self, node: AudioNode) -> usize {
        self.shape(node).map_or(0, |s| s.audio_in.count() as usize)
    }

    /// `node`'s output port count.
    pub fn node_outputs(&self, node: AudioNode) -> usize {
        self.shape(node).map_or(0, |s| s.audio_out.count() as usize)
    }

    /// The latency `node` reports: the shape the editor holds, read when the
    /// node was inserted (prepared at the graph's rate) and moved since only
    /// by a re-prepare or a hosted plugin's latency change.
    pub fn node_latency(&self, node: AudioNode) -> Samples {
        self.shape(node).map_or(Samples(0), |s| s.latency.samples())
    }

    /// The tail `node` reports.
    pub fn node_tail(&self, node: AudioNode) -> Tail {
        self.shape(node).map_or(Tail::None, |s| s.tail)
    }

    /// Widen the global outputs to `channels`. New channels read silence.
    /// Never narrows.
    pub fn widen_outputs(&mut self, channels: usize) {
        let outputs = &mut self.editor.spec_mut().topology.outputs;
        if outputs.len() < channels {
            outputs.resize(channels, Source::Zero);
            self.edited = true;
        }
    }

    // --- Latency ---

    /// What compensation this graph needs, folded over the graph as edited
    /// (committed or not): audio edges plus each node's event and
    /// param-modulation sources. Mutates nothing.
    pub fn latency_plan(&self) -> Compensation {
        tutti_types::latency::plan(self.editor.spec())
    }

    /// The compensation the next commit's plan carries: the spec compiled
    /// against the shapes, as the commit will compile it, as the per-channel
    /// pre-roll and the total. Nothing is inserted — PDC is the compiler's.
    /// `None` when the spec does not compile (the commit will say why).
    pub fn planned_compensation(&self) -> Option<(Vec<Samples>, Samples)> {
        let valid = self.editor.spec().validate().ok()?;
        let (plan, _) = tutti_graph::compile(
            &valid,
            self.editor.shapes(),
            self.editor.prepare(),
            self.editor.base().map(|p| &**p),
        )
        .ok()?;
        Some((plan.compensation().to_vec(), plan.total_latency().samples()))
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

    /// The compensation of the plan sent last, as the per-channel pre-roll
    /// and the total: what the session publishes, since it is what the
    /// executor is handed.
    pub fn sent_compensation(&self) -> Option<(Vec<Samples>, Samples)> {
        let plan = self.editor.base()?;
        Some((plan.compensation().to_vec(), plan.total_latency().samples()))
    }

    /// A node's latency moved at runtime (a plugin's latency cell): hand
    /// `latency` — the figure the node's `Shape` declares now, which for a
    /// plugin is `PluginControls::declared_latency` — to the editor
    /// (`Editor::set_latency`), clamped to what PDC compensates, so the next
    /// commit moves PDC to it without touching the running node.
    ///
    /// Returns whether the editor's figure moved.
    #[cfg(feature = "plugin")]
    pub fn refresh_node_latency(&mut self, node: AudioNode, latency: Latency) -> bool {
        if !self.nodes.contains_key(&key(node)) {
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
        match self.editor.set_latency(key(node), latency) {
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

    /// Drain what the executor sent back, freeing retired units here.
    pub(crate) fn collect(&mut self) {
        self.editor.collect();
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
pub enum AudioNodeTarget {
    /// The master: every global output.
    Master,
    /// `node`'s outputs.
    Node(AudioNode),
}

/// Resolves a host's modulation keys to graph nodes.
///
/// `tutti_mod`'s declarations key a target by `TargetKey { node: u64, param }`
/// and a source by `SourceKey(u64)`; the `u64` is the host's own minting (a
/// Bevy entity's bits, say), and this is how [`Graph::modulate`] reads it.
#[cfg(feature = "modulation")]
pub trait ModKeys {
    /// The node a target's key names.
    fn node(&self, target: u64) -> Option<AudioNode>;
    /// Plugin parameter `id`'s value on `target`'s node: `None` while not
    /// read yet, `Some(None)` for a parameter it does not list. Temporary:
    /// it goes once the session owns the plugin mirror (design 015 PR 6).
    fn plugin_value(&self, target: u64, id: u32) -> Option<Option<f32>>;
    /// What feeds source `source`'s node.
    fn feed(&self, source: tutti_mod::SourceKey) -> Option<tutti_nodes::Feed>;
}

/// `tutti_nodes::ModHost` over the graph's nodes and the host's keys: a
/// target's key names its node, a native target's knob is that node's
/// `Param` cell, a plugin target feeds the node's event input 0 and starts at
/// the plugin's mirrored value.
#[cfg(feature = "modulation")]
struct ModHostOver<'a> {
    nodes: &'a BTreeMap<NodeKey, Entry>,
    keys: &'a dyn ModKeys,
}

#[cfg(feature = "modulation")]
impl tutti_nodes::ModHost for ModHostOver<'_> {
    fn target(&self, target: tutti_mod::TargetKey) -> Option<tutti_nodes::Target> {
        let node = key(self.keys.node(target.node)?);
        Some(match target.param {
            tutti_types::ParamAddr::Unit(param) => tutti_nodes::Target::Native {
                node,
                knob: self
                    .nodes
                    .get(&node)
                    .and_then(|e| e.params.as_ref())
                    .and_then(|p| p.cell(param)),
            },
            // A plugin target waits for its mirror: a lane built without
            // a knob could not take one later.
            tutti_types::ParamAddr::Id(id) => tutti_nodes::Target::Foreign {
                node,
                port: 0,
                knob: self.keys.plugin_value(target.node, id)?,
            },
        })
    }

    fn feed(&self, source: tutti_mod::SourceKey) -> Option<tutti_nodes::Feed> {
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
pub struct PendingExport {
    fork: tutti_graph::PendingFork,
    pool: Option<Arc<dyn tutti_graph::Pool>>,
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
    pub fn build(
        self,
    ) -> Result<tutti_export::RenderGraph, (tutti_export::Error, tutti_graph::ForkRemains)> {
        let mut graph = tutti_export::RenderGraph::build(self.fork)?;
        graph.set_pool(self.pool);
        Ok(graph)
    }
}
