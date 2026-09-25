//! Network of audio units connected together.

use super::audionode::*;
use super::audiounit::*;
use super::buffer::*;
use super::combinator::*;
use super::math::*;
use super::realnet::*;
use super::sequencer::Fade;
use super::setting::*;
use super::shared::IdGenerator;
use super::signal::*;
use super::vertex::*;
use super::*;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use hashbrown::HashMap;
use tutti_types::Tail;

// Iterator type returned from `Net::ids`.
pub use hashbrown::hash_map::Keys;

/// Network errors. These are accessible via `Net::error`.
/// The only error so far is a connection cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetError {
    /// A connection cycle was detected.
    Cycle,
}

impl core::fmt::Display for NetError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Net has one or more cycles")
    }
}

pub type NodeIndex = usize;
pub type PortIndex = usize;

/// Globally unique node ID for a node in a network.
#[derive(PartialEq, Eq, Hash, Clone, Copy, Debug, Default)]
pub struct NodeId(u64);

/// This atomic supplies globally unique node IDs.
static GLOBAL_NODE_ID: IdGenerator = IdGenerator::new();

impl NodeId {
    /// Create a new, globally unique node ID.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        NodeId(GLOBAL_NODE_ID.get_id())
    }
    /// Return the raw value of the ID.
    /// This is provided in case the user wishes to use the same ID for another purpose.
    pub fn value(&self) -> u64 {
        self.0
    }
}

impl core::fmt::Display for NodeId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "n{}", self.0)
    }
}

// `Setting`'s node address is `tutti_node::setting::NodeAddr`, an opaque `u64`
// with no generator of its own, so that the node *contract* does not have to
// name this file's global counter. These two impls are the whole boundary: the
// minting stays here, where the nodes are, and the address the contract carries
// is just the bits.
//
// Total and lossless in both directions — `NodeId` is a `u64` newtype and so is
// `NodeAddr`, so neither direction can fail or lose a bit. That matters because
// `Net::set` compares a converted address against a converted key: a conversion
// that dropped information would route a setting to the wrong node, or to none,
// and `Net` reports the latter only as a counter.
impl From<NodeId> for tutti_node::setting::NodeAddr {
    #[inline]
    fn from(id: NodeId) -> Self {
        tutti_node::setting::NodeAddr::new(id.0)
    }
}

impl From<tutti_node::setting::NodeAddr> for NodeId {
    #[inline]
    fn from(addr: tutti_node::setting::NodeAddr) -> Self {
        NodeId(addr.get())
    }
}

/// Node introduced with a crossfade.
#[derive(Clone, Default)]
pub(crate) struct NodeEdit {
    pub unit: Option<Box<dyn AudioUnit>>,
    pub id: NodeId,
    pub index: NodeIndex,
    pub fade: Fade,
    pub fade_time: f32,
}

// Net type ID for pseudorandom phase.
const ID: u64 = 63;

/// Input or output port.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum Port {
    /// Node input or output.
    Local(NodeIndex, PortIndex),
    /// Network input or output.
    Global(PortIndex),
    /// Unconnected input. Unconnected output ports are not marked anywhere.
    #[default]
    Zero,
}

/// Source for an input or source for a global output.
/// The complete graph consists of nodes with their input edges and global output edges.
/// This is a user facing structure.
/// Source can be a contained node output (`Source::Local`), a network input (`Source::Global`)
/// or zeros (`Source::Zero`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Source {
    /// Node output.
    Local(NodeId, PortIndex),
    /// Network input.
    Global(PortIndex),
    /// Unconnected input or global output.
    #[default]
    Zero,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct NetEdge {
    pub source: Port,
    pub target: Port,
}

/// Create an edge from source to target.
pub(crate) fn edge(source: Port, target: Port) -> NetEdge {
    NetEdge { source, target }
}

/// Network unit. It can contain other units and maintain connections between them.
/// Outputs of the network are sourced from user specified unit outputs or
/// global inputs, or are filled with zeros if not connected.
#[derive(Default)]
pub struct Net {
    /// Global input buffers.
    input: BufferVec,
    /// Global output buffers.
    output: BufferVec,
    /// Sources of global outputs.
    output_edge: Vec<NetEdge>,
    /// Vertices of the graph.
    vertex: Vec<Vertex>,
    /// Ordering of vertex evaluation.
    order: Option<Vec<NodeIndex>>,
    /// Translation map from node ID to vertex index.
    node_index: HashMap<NodeId, NodeIndex>,
    /// Current sample rate. `f64` so the cascade to child units does not lose
    /// precision. Vertex tick/process still take `f32` (FunDSP-internal SIMD
    /// convention); the cast happens at the call seam below, exactly once.
    sample_rate: f64,
    /// Optional frontend.
    front: Option<(Arc<Queue<NetMessage, 256>>, Arc<Queue<NetReturn, 256>>)>,
    /// Number of inputs in the backend. This is for checking consistency during commits.
    backend_inputs: usize,
    /// Number of outputs in the backend. This is for checking consistency during commits.
    backend_outputs: usize,
    /// Queue of smooth edits made to nodes. Applicable to frontends only.
    edit_queue: Vec<NodeEdit>,
    /// Revision number. This is used by frontends and backends only.
    /// The revision is incremented after each commit.
    revision: u64,
    /// Current error, if any.
    error: Option<NetError>,
    /// Settings the message queue had no room for, since the last
    /// [`take_dropped_settings`](Self::take_dropped_settings).
    ///
    /// A dropped setting is a parameter change that never reaches the audio
    /// thread — a fader that moves on screen and not in the sound. Unlike a
    /// retired unit it cannot be parked, because the next setting for the same
    /// target supersedes it and replaying a stale one would be worse than losing
    /// it. Counting is what is left: silence here is what made this
    /// indistinguishable from a working fader.
    dropped_settings: u64,
    /// Settings that named a node this network does not contain, since the last
    /// [`take_unaddressed_settings`](Self::take_unaddressed_settings).
    ///
    /// **A different failure from `dropped_settings`, with the same symptom.** A
    /// dropped setting is transient backpressure — the queue was full, the value
    /// was real, the next one gets through. An unaddressed setting is a *bug*: the
    /// address does not resolve and never will, so every future write to that
    /// target is lost too. Counting them apart is what makes "the fader is ahead
    /// of the audio thread" distinguishable from "this fader is wired to nothing".
    ///
    /// Shared rather than owned, because **the misaddress is detected on the
    /// backend**. `Net::set` on a frontend enqueues without inspecting the
    /// address; the backend then calls `set` on its own front-less `Net`, takes
    /// the resolving branch, and finds nothing. A plain `u64` there would be
    /// stranded on the audio thread, which is the one place a host cannot read.
    ///
    /// So `backend()` hands the backend's `Net` a clone of this `Arc`, and
    /// [`take_unaddressed_settings`](Self::take_unaddressed_settings) on the
    /// frontend reads what the audio thread counted. Incremented with `Relaxed`:
    /// the count is a diagnostic, not a synchronisation edge, and nothing
    /// downstream orders against it.
    unaddressed_settings: Arc<AtomicU64>,
}

impl Clone for Net {
    fn clone(&self) -> Self {
        Self {
            input: self.input.clone(),
            output: self.output.clone(),
            output_edge: self.output_edge.clone(),
            vertex: self.vertex.clone(),
            order: self.order.clone(),
            node_index: self.node_index.clone(),
            sample_rate: self.sample_rate,
            // Frontend is never cloned.
            front: None,
            backend_inputs: self.backend_inputs,
            backend_outputs: self.backend_outputs,
            // Edit queue belongs to the frontend and is never cloned.
            edit_queue: Vec::new(),
            revision: self.revision,
            error: self.error.clone(),
            // The count belongs to the frontend that dropped them, like
            // `front` and `edit_queue`.
            dropped_settings: 0,
            // A fresh cell, not a shared one: a clone is a *separate* graph with
            // its own `node_index`, so what is unaddressable in it is a different
            // question. `backend()` overwrites this with the frontend's cell
            // afterwards, because there the two halves are one network.
            unaddressed_settings: Arc::new(AtomicU64::new(0)),
        }
    }
}

/// A freshly cloned [`Net`] that has **not** been severed from the live graph.
///
/// Cloning a running network is not the same as getting a private copy of it.
/// Nodes hold shared live state — an `Arc`-shared vocoder bank in a time
/// stretcher, a live command `Receiver` in a voice pool, a live `Timeline` in a
/// clock — and until every node's [`AudioUnit::isolate`] has run, touching the
/// clone reaches through into the graph the audio thread is using.
///
/// This type exists so that cannot be forgotten. It implements nothing: no
/// [`AudioUnit`], no `Deref`, no accessor. The only way to get a usable [`Net`]
/// out is [`isolate`](Self::isolate), so "isolate before you touch it" is a
/// compile error rather than a review comment.
///
/// # Why it is a type and not a doc comment
///
/// It used to be a doc comment. The offline region render did
/// `clone_isolated` → `reset()` → isolate-per-node, and the `reset()` in the
/// middle cleared the FIFOs and phase accumulators of the *live* stretch units
/// through their shared banks — measured as the live voice's output dropping to
/// exactly 0.0 for one window. Nothing in the types objected, because a `Net`
/// that is unsafe to use is indistinguishable from one that is safe.
#[must_use = "a PendingClone shares state with the live graph until isolated"]
pub struct PendingClone(Net);

impl PendingClone {
    /// Sever every node from the live graph, yielding a usable [`Net`].
    ///
    /// Each unit severs whatever live input it holds; the default
    /// [`AudioUnit::isolate`] is a no-op, which is correct for pure-DSP nodes
    /// that share nothing. A node type that *does* hold shared state and does
    /// not implement `isolate` is a bug in that node — this type guarantees the
    /// call happens, not that each implementation is right.
    pub fn isolate(mut self) -> Net {
        let ids: Vec<NodeId> = self.0.ids().copied().collect();
        for id in ids {
            self.0.node_mut(id).isolate();
        }
        self.0
    }

    /// Sever every node *and* re-point it at an offline render's data.
    ///
    /// The two halves of preparing an offline clone, in the order they must
    /// happen: [`AudioUnit::isolate`] drops shared live inputs, then
    /// [`AudioUnit::rebind_offline`] re-seats whatever transport the severed
    /// node still points at. `ctx` is opaque here and downcast by each
    /// implementor — see `rebind_offline`'s docs for why.
    ///
    /// Prefer this over `isolate()` for anything that will be *rendered*.
    /// `isolate()` alone leaves transport-aware nodes aiming at the live
    /// playhead, which nothing advances offline, so they render silence.
    pub fn isolate_for_offline(mut self, ctx: &dyn core::any::Any) -> Net {
        let ids: Vec<NodeId> = self.0.ids().copied().collect();
        for id in ids {
            let node = self.0.node_mut(id);
            node.isolate();
            node.rebind_offline(ctx);
        }
        self.0
    }
}

impl Net {
    /// Create a new network with the given number of inputs and outputs.
    /// The number of inputs and outputs is fixed after construction.
    /// Network global outputs are initialized to zero.
    ///
    /// ### Example (Sine Oscillator)
    /// ```
    /// use fundsp_tutti::prelude64::*;
    /// let mut net = Net::new(1, 1);
    /// net.chain(Box::new(sine()));
    /// net.check();
    /// ```
    pub fn new(inputs: usize, outputs: usize) -> Self {
        let mut net = Self {
            unaddressed_settings: Arc::new(AtomicU64::new(0)),
            input: BufferVec::new(inputs),
            output: BufferVec::new(outputs),
            output_edge: Vec::with_capacity(outputs),
            vertex: Vec::new(),
            order: None,
            node_index: HashMap::new(),
            sample_rate: DEFAULT_SR,
            front: None,
            backend_inputs: inputs,
            backend_outputs: outputs,
            edit_queue: Vec::new(),
            revision: 0,
            error: None,
            dropped_settings: 0,
        };
        for channel in 0..outputs {
            net.output_edge
                .push(edge(Port::Zero, Port::Global(channel)));
        }
        net
    }

    /// Create a network with `channels` outputs, no inputs, and a live
    /// backend already taken.
    ///
    /// [`new`](Self::new) leaves the network without a backend, so
    /// [`commit`](Self::commit) has nowhere to publish. This is the shape the
    /// engine builder produces — convenient for tests and bootstrap graphs that
    /// commit but never drive audio (the backend is dropped).
    pub fn with_backend(channels: usize) -> Self {
        let mut net = Self::new(0, channels);
        let _backend = net.backend();
        net
    }

    /// Number of settings dropped because the message queue was full, resetting
    /// the count to zero.
    ///
    /// Non-zero means parameter changes were made that the audio thread never
    /// saw: the queue holds 256 messages and drains once per block, so a burst
    /// of automation between two blocks can outrun it. A host that polls this
    /// can re-send the affected parameters or warn; ignoring it reproduces the
    /// stuck fader this counter exists to make visible.
    pub fn take_dropped_settings(&mut self) -> u64 {
        core::mem::take(&mut self.dropped_settings)
    }

    /// Number of settings this network could not route, resetting the count to
    /// zero.
    ///
    /// Non-zero means a parameter is wired to **nothing** — not merely late. The
    /// sibling counter [`take_dropped_settings`](Self::take_dropped_settings)
    /// reports a full queue, which is transient and self-correcting; this one
    /// reports an address that does not resolve, so every future write to that
    /// target is lost the same way. A host that polls this can name the offending
    /// parameter instead of leaving it to present as a dead fader.
    ///
    /// Counts **every** address a `Net` cannot act on, which is wider than a bad
    /// [`NodeId`]: `Net` routes [`Address::Node`] alone, so `Index`, `Left`,
    /// `Right` and the default `Null` are unroutable here too. Those are
    /// meaningful on other unit types — `Setting::interval(t)` carries no address
    /// and a combinator resolves `left`/`right` — but reaching a `Net` with one
    /// means the setting was addressed for a different shape of unit. It was
    /// already discarded before this counter existed; now it is visible.
    ///
    /// # Startup is not steady state
    ///
    /// **A non-zero count right after `backend()` is expected, not a bug.** The
    /// backend is cloned from the network as it stands, so its `node_index` is
    /// empty until the first [`commit`](Self::commit) reaches it — and a host
    /// that pushes a param the same frame it spawns a node addresses one the
    /// audio thread has not seen yet. That setting is counted here and then
    /// superseded by the next push once the node lands.
    ///
    /// So poll this against a *settled* graph, or drain it once after startup.
    /// A host that alarms on the first non-zero reading will alarm on every
    /// launch. What the counter is worth watching for is a count that keeps
    /// climbing while the graph is quiet — that is a target nothing will ever
    /// resolve.
    ///
    /// Readable on the **frontend** even though the miss is detected on the audio
    /// thread: the counter is an `Arc<AtomicU64>` shared with the backend at
    /// [`backend`](Self::backend). Taking it here clears what the audio thread
    /// counted.
    pub fn take_unaddressed_settings(&mut self) -> u64 {
        self.unaddressed_settings.swap(0, Ordering::Relaxed)
    }

    /// Return current error condition, if any.
    /// The only possible error so far is a connection cycle.
    /// If all cycles are removed, then the error will be cleared.
    /// This call computes network topology to detect cycles.
    pub fn error(&mut self) -> &Option<NetError> {
        if !self.is_ordered() {
            self.determine_order();
        }
        &self.error
    }

    /// Add a new unit to the network. Return its ID handle.
    /// Unit inputs are initially set to zero.
    /// `Net::fade_in` is a smooth version of this method.
    ///
    /// ### Example (Sine Oscillator)
    /// ```
    /// use fundsp_tutti::prelude64::*;
    /// let mut net = Net::new(1, 1);
    /// let id = net.push(Box::new(sine()));
    /// net.pipe_input(id);
    /// net.pipe_output(id);
    /// net.check();
    /// ```
    pub fn push(&mut self, mut unit: Box<dyn AudioUnit>) -> NodeId {
        unit.set_sample_rate(crate::SampleRate(self.sample_rate));
        let index = self.vertex.len();
        let id = NodeId::new();
        let vertex = Vertex::new(id, index, unit);
        self.vertex.push(vertex);
        self.node_index.insert(id, index);
        self.invalidate_order();
        id
    }

    /// Add a new unit to the network, boxing it. Return its ID handle.
    ///
    /// Convenience over [`push`](Self::push) for the common case of a unit that
    /// is not already boxed — `net.add(sine_hz(440.0))` rather than
    /// `net.push(Box::new(sine_hz(440.0)))`. Callers holding a
    /// `Box<dyn AudioUnit>` (plugin loaders, trait-object factories) want
    /// `push`.
    pub fn add<U: AudioUnit + 'static>(&mut self, unit: U) -> NodeId {
        self.push(Box::new(unit))
    }

    /// Add `unit` and wire all of its outputs to the global output bus,
    /// returning its id. [`add`](Self::add) + [`pipe_output`](Self::pipe_output).
    pub fn master<U: AudioUnit + 'static>(&mut self, unit: U) -> NodeId {
        let id = self.add(unit);
        self.pipe_output(id);
        id
    }

    /// Boxed-input variant of [`master`](Self::master).
    pub fn master_boxed(&mut self, unit: Box<dyn AudioUnit>) -> NodeId {
        let id = self.push(unit);
        self.pipe_output(id);
        id
    }

    /// Clone the network with its global output bus repointed at `target`, so
    /// rendering the clone captures `target`'s signal post-everything-upstream.
    ///
    /// A mono target fans to every bus channel; a wider target maps port→channel
    /// and clamps any extra bus channels to the target's last port. The upstream
    /// cone keeps ticking — *all* vertices are ordered, not just those reachable
    /// from the output bus (running nodes may have side effects) — so nothing is
    /// pruned. Sibling branches that no longer feed an output are never read.
    ///
    /// Returns `None` if `target` produces no output channels. Clones rather
    /// than mutating in place because this rewrites the output edges; the live
    /// network keeps its own.
    pub fn clone_isolated(&self, target: NodeId) -> Option<PendingClone> {
        let outs = self.outputs_in(target);
        if outs == 0 {
            return None;
        }
        let mut clone = self.clone();
        for ch in 0..clone.outputs() {
            // Mono target fans to every bus channel; multi-out target maps
            // port→ch, clamping extra bus channels to the target's last port.
            let port = core::cmp::min(ch, outs - 1);
            clone.set_output_source(ch, Source::Local(target, port));
        }
        Some(PendingClone(clone))
    }

    /// Add a new unit to the network with a fade-in. Return its ID handle.
    /// Unit inputs are initially set to zero.
    pub fn fade_in(&mut self, fade: Fade, fade_time: f32, unit: Box<dyn AudioUnit>) -> NodeId {
        let dummy = DummyUnit::new(unit.inputs(), unit.outputs());
        let id = self.push(Box::new(dummy));
        self.crossfade(id, fade, fade_time, unit);
        id
    }

    /// Return an iterator over the node IDs of the network.
    /// The nodes are iterated in an arbitrary order.
    pub fn ids(&self) -> Keys<'_, NodeId, usize> {
        self.node_index.keys()
    }

    /// Monotonic revision counter, incremented on every commit and on each
    /// staged edit. Two nets with the same revision describe the same graph;
    /// a change in revision means the rendered output may differ. Useful as a
    /// cache-invalidation key for anything derived from the graph's audio.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The sample rate every contained unit was last set to.
    ///
    /// Read counterpart to [`AudioUnit::set_sample_rate`], which is what
    /// writes it. Callers that snapshot a net and re-rate the copy (offline
    /// render) can read back what they imposed rather than tracking it
    /// alongside.
    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// Get the signal source for `node` input `channel`.
    /// Sources can be network inputs (`Source::Global`), node outputs (`Source::Local`) or zeros (`Source::Zero`).
    /// The complete graph consists of contained nodes and edges from here and the ones from `output_source`.
    pub fn source(&self, node: NodeId, channel: usize) -> Source {
        let index = self.node_index[&node];
        assert!(channel < self.vertex[index].inputs());
        match self.vertex[index].source[channel].source {
            Port::Global(i) => Source::Global(i),
            Port::Local(i, j) => Source::Local(self.vertex[i].id, j),
            Port::Zero => Source::Zero,
        }
    }

    /// Set the signal source for `node` input `channel`.
    /// Self connections are prohibited. Creating cycles will result in a recoverable error condition (see `Net::error)`.
    /// Sources can be network inputs (`Source::Global`), node outputs (`Source::Local`) or zeros (`Source::Zero`).
    /// The complete graph consists of contained nodes and edges from here and the ones from `set_output_source`.
    pub fn set_source(&mut self, node: NodeId, channel: usize, source: Source) {
        let index = self.node_index[&node];
        assert!(channel < self.vertex[index].inputs());
        self.vertex[index].source[channel].source = match source {
            Source::Global(i) => {
                assert!(i < self.inputs());
                Port::Global(i)
            }
            Source::Local(id, j) => {
                assert!(id != node);
                let i = self.node_index[&id];
                assert!(j < self.vertex[i].outputs());
                Port::Local(i, j)
            }
            Source::Zero => Port::Zero,
        };
        self.invalidate_order();
    }

    /// Get the signal source for network output `channel`.
    /// Sources can be network inputs (`Source::Global`), node outputs (`Source::Local`) or zeros (`Source::Zero`).
    /// The complete graph consists of contained nodes and edges from here and the ones from `source`.
    pub fn output_source(&self, channel: usize) -> Source {
        assert!(channel < self.outputs());
        match self.output_edge[channel].source {
            Port::Global(i) => Source::Global(i),
            Port::Local(i, j) => Source::Local(self.vertex[i].id, j),
            Port::Zero => Source::Zero,
        }
    }

    /// Set the signal source for network output `channel`.
    /// Sources can be network inputs (`Source::Global`), node outputs (`Source::Local`) or zeros (`Source::Zero`).
    /// The complete graph consists of contained nodes and edges from here and the ones from `set_source`.
    pub fn set_output_source(&mut self, channel: usize, source: Source) {
        assert!(channel < self.outputs());
        self.output_edge[channel].source = match source {
            Source::Global(i) => {
                assert!(i < self.inputs());
                Port::Global(i)
            }
            Source::Local(id, j) => {
                let i = self.node_index[&id];
                assert!(j < self.vertex[i].outputs());
                Port::Local(i, j)
            }
            Source::Zero => Port::Zero,
        };
        self.invalidate_order();
    }

    /// Whether we have calculated the order vector.
    #[inline]
    fn is_ordered(&self) -> bool {
        self.order.is_some()
    }

    /// Invalidate any previously calculated order.
    #[inline]
    fn invalidate_order(&mut self) {
        self.order = None;
    }

    /// Remove `node` from network. Returns the unit that was removed.
    /// All connections from the unit are replaced with zeros.
    /// If this is a frontend, then the returned unit is a clone.
    ///
    /// ### Example (Sine Oscillator)
    /// ```
    /// use fundsp_tutti::prelude64::*;
    /// let mut net = Net::new(1, 1);
    /// let id1 = net.push(Box::new(sine()));
    /// let id2 = net.push(Box::new(sine()));
    /// net.connect_input(0, id2, 0);
    /// net.connect_output(id2, 0, 0);
    /// net.remove(id1);
    /// assert!(net.size() == 1);
    /// net.check();
    /// ```
    pub fn remove(&mut self, node: NodeId) -> Box<dyn AudioUnit> {
        self.remove_2(node, false)
    }

    /// Remove `node` from network. Returns the unit that was removed.
    /// Connections from the unit are replaced with pass-through connections.
    /// The unit must have an equal number of inputs and outputs.
    /// If this is a frontend, then the returned unit is a clone.
    ///
    /// ### Example
    /// ```
    /// use fundsp_tutti::prelude64::*;
    /// let mut net = Net::new(1, 1);
    /// let id1 = net.chain(Box::new(add(1.0)));
    /// let id2 = net.chain(Box::new(add(2.0)));
    /// assert!(net.size() == 2);
    /// assert!(net.filter_mono(1.0) == 4.0);
    /// net.remove_link(id2);
    /// assert!(net.size() == 1);
    /// assert!(net.filter_mono(1.0) == 2.0);
    /// net.check();
    /// ```
    pub fn remove_link(&mut self, node: NodeId) -> Box<dyn AudioUnit> {
        self.remove_2(node, true)
    }

    /// Remove `node` from network. If `link` is false then connections from the unit
    /// are replaced with zeros; if `link` is true then connections are replaced
    /// by matching inputs of the unit, and the number of inputs must be equal to the number of outputs.
    fn remove_2(&mut self, node: NodeId, link: bool) -> Box<dyn AudioUnit> {
        let node_index = self.node_index[&node];
        assert!(!link || self.vertex[node_index].inputs() == self.vertex[node_index].outputs());
        // Replace all global ports that use an output of the node.
        for channel in 0..self.outputs() {
            if let Port::Local(index, port) = self.output_edge[channel].source
                && index == node_index
            {
                self.output_edge[channel].source = if link {
                    self.vertex[node_index].source[port].source
                } else {
                    Port::Zero
                };
            }
        }
        // Replace all local ports that use an output of the node.
        for vertex in 0..self.size() {
            for channel in 0..self.vertex[vertex].inputs() {
                if let Port::Local(index, port) = self.vertex[vertex].source[channel].source
                    && index == node_index
                {
                    self.vertex[vertex].source[channel].source = if link {
                        self.vertex[node_index].source[port].source
                    } else {
                        Port::Zero
                    };
                }
            }
        }
        self.node_index.remove(&self.vertex[node_index].id);
        let last_index = self.size() - 1;
        if last_index != node_index {
            // Move node from `last_index` to `node_index`.
            self.vertex.swap(node_index, last_index);
            self.node_index
                .insert(self.vertex[node_index].id, node_index);
            for channel in 0..self.outputs() {
                if let Port::Local(index, port) = self.output_edge[channel].source
                    && index == last_index
                {
                    self.output_edge[channel].source = Port::Local(node_index, port);
                }
            }
            for vertex in 0..self.size() - 1 {
                for channel in 0..self.vertex[vertex].inputs() {
                    if let Port::Local(index, port) = self.vertex[vertex].source[channel].source
                        && index == last_index
                    {
                        self.vertex[vertex].source[channel].source = Port::Local(node_index, port);
                    }
                }
            }
            for channel in 0..self.vertex[node_index].inputs() {
                self.vertex[node_index].source[channel].target = Port::Local(node_index, channel);
            }
        }
        self.invalidate_order();

        self.vertex.pop().unwrap().unit
    }

    /// Replaces the given node in the network.
    /// All connections are retained.
    /// The replacement must have the same number of inputs and outputs
    /// as the node it is replacing.
    /// The ID of the node remains the same.
    /// Returns the unit that was replaced.
    /// If this network is a frontend, then the returned unit is a clone.
    /// `Net::crossfade` is a smooth version of this method.
    ///
    /// ### Example (Replace Saw Wave With Square Wave)
    /// ```
    /// use fundsp_tutti::prelude32::*;
    /// let mut net = Net::new(0, 1);
    /// let id = net.push(Box::new(saw_hz(220.0)));
    /// net.pipe_output(id);
    /// net.replace(id, Box::new(square_hz(220.0)));
    /// net.check();
    /// ```
    pub fn replace(&mut self, node: NodeId, mut unit: Box<dyn AudioUnit>) -> Box<dyn AudioUnit> {
        let node_index = self.node_index[&node];
        assert_eq!(unit.inputs(), self.vertex[node_index].inputs());
        assert_eq!(unit.outputs(), self.vertex[node_index].outputs());
        unit.set_sample_rate(crate::SampleRate(self.sample_rate));
        core::mem::swap(&mut self.vertex[node_index].unit, &mut unit);
        self.vertex[node_index].changed = self.revision;
        unit
    }

    /// Replaces the given node in the network smoothly with a crossfade.
    /// All connections are retained.
    /// The replacement must have the same number of inputs and outputs
    /// as the node it is replacing.
    /// The ID of the node remains the same.
    ///
    /// ### Example (Replace Saw Wave With Square Wave Via 1 Second Crossfade)
    /// ```
    /// use fundsp_tutti::prelude32::*;
    /// let mut net = Net::new(0, 1);
    /// let id = net.push(Box::new(saw_hz(220.0)));
    /// net.pipe_output(id);
    /// net.crossfade(id, Fade::Smooth, 1.0, Box::new(square_hz(220.0)));
    /// net.check();
    /// ```
    pub fn crossfade(
        &mut self,
        node: NodeId,
        fade: Fade,
        fade_time: f32,
        mut unit: Box<dyn AudioUnit>,
    ) {
        let node_index = self.node_index[&node];
        assert_eq!(unit.inputs(), self.vertex[node_index].inputs());
        assert_eq!(unit.outputs(), self.vertex[node_index].outputs());
        unit.set_sample_rate(crate::SampleRate(self.sample_rate));
        unit.allocate();
        let mut edit = NodeEdit {
            unit: Some(unit),
            id: node,
            index: node_index,
            fade,
            fade_time,
        };
        if self.has_backend() {
            self.edit_queue.push(edit);
        } else {
            self.vertex[node_index].enqueue(&mut edit, &None);
        }
    }

    /// Connect the given unit output (`source`, `source_port`)
    /// to the given unit input (`target`, `target_port`).
    /// There is one connection for each unit input.
    ///
    /// ### Example (Filtered Saw Oscillator)
    /// ```
    /// use fundsp_tutti::prelude64::*;
    /// let mut net = Net::new(1, 1);
    /// let id1 = net.push(Box::new(saw()));
    /// let id2 = net.push(Box::new(lowpass_hz(1000.0, 1.0)));
    /// net.connect(id1, 0, id2, 0);
    /// net.pipe_input(id1);
    /// net.pipe_output(id2);
    /// net.check();
    /// ```
    pub fn connect(
        &mut self,
        source: NodeId,
        source_port: PortIndex,
        target: NodeId,
        target_port: PortIndex,
    ) {
        assert!(source != target);
        let source_index = self.node_index[&source];
        let target_index = self.node_index[&target];
        self.connect_index(source_index, source_port, target_index, target_port);
    }

    /// Disconnect `node` input `port`, replacing it with zero input.
    ///
    /// ### Example
    /// ```
    /// use fundsp_tutti::prelude64::*;
    /// let mut net = Net::new(1, 1);
    /// let id = net.chain(Box::new(pass()));
    /// assert!(net.filter_mono(1.0) == 1.0);
    /// net.disconnect(id, 0);
    /// assert!(net.filter_mono(1.0) == 0.0);
    /// net.check();
    /// ```
    pub fn disconnect(&mut self, node: NodeId, port: PortIndex) {
        let node_index = self.node_index[&node];
        self.vertex[node_index].source[port].source = Port::Zero;
        self.invalidate_order();
    }

    /// Connect the given unit output (`source`, `source_port`)
    /// to the given unit input (`target`, `target_port`).
    fn connect_index(
        &mut self,
        source: NodeIndex,
        source_port: PortIndex,
        target: NodeIndex,
        target_port: PortIndex,
    ) {
        self.vertex[target].source[target_port] = edge(
            Port::Local(source, source_port),
            Port::Local(target, target_port),
        );
        self.invalidate_order();
    }

    /// Connect the node input (`target`, `target_port`)
    /// to the network input `global_input`.
    ///
    /// ### Example (Saw Wave)
    /// ```
    /// use fundsp_tutti::prelude64::*;
    /// let mut net = Net::new(1, 1);
    /// let id = net.push(Box::new(saw()));
    /// net.connect_input(0, id, 0);
    /// net.connect_output(id, 0, 0);
    /// net.check();
    /// ```
    pub fn connect_input(
        &mut self,
        global_input: PortIndex,
        target: NodeId,
        target_port: PortIndex,
    ) {
        let target_index = self.node_index[&target];
        self.connect_input_index(global_input, target_index, target_port);
    }

    /// Connect the node input (`target`, `target_port`)
    /// to the network input `global_input`.
    fn connect_input_index(
        &mut self,
        global_input: PortIndex,
        target: NodeIndex,
        target_port: PortIndex,
    ) {
        self.vertex[target].source[target_port] =
            edge(Port::Global(global_input), Port::Local(target, target_port));
        self.invalidate_order();
    }

    /// Pipe global input to node `target`.
    /// If there are fewer global inputs than inputs in `target`,
    /// then a modulo operation is taken to obtain the global input.
    /// If there are more global inputs than inputs in `target`, then only the first ones are used.
    /// If there are no global inputs at all, then zeros are supplied.
    /// ### Example (Stereo Filter)
    /// ```
    /// use fundsp_tutti::prelude64::*;
    /// let mut net = Net::new(2, 2);
    /// let id = net.push(Box::new(peak_hz(1000.0, 1.0) | peak_hz(1000.0, 1.0)));
    /// net.pipe_input(id);
    /// net.pipe_output(id);
    /// net.check();
    /// ```
    pub fn pipe_input(&mut self, target: NodeId) {
        let target_index = self.node_index[&target];
        let global_inputs = self.inputs();
        for channel in 0..self.vertex[target_index].inputs() {
            if global_inputs > 0 {
                self.vertex[target_index].source[channel] = edge(
                    Port::Global(channel % global_inputs),
                    Port::Local(target_index, channel),
                );
            } else {
                self.vertex[target_index].source[channel] =
                    edge(Port::Zero, Port::Local(target_index, channel));
            }
        }
        self.invalidate_order();
    }

    /// Connect node output (`source`, `source_port`) to network output `global_output`.
    /// There is one connection for each global output.
    pub fn connect_output(
        &mut self,
        source: NodeId,
        source_port: PortIndex,
        global_output: PortIndex,
    ) {
        let source_index = self.node_index[&source];
        self.connect_output_index(source_index, source_port, global_output);
    }

    /// Disconnect global `output`. Replaces output with zero signal.
    pub fn disconnect_output(&mut self, output: PortIndex) {
        self.output_edge[output] = edge(Port::Zero, Port::Global(output));
        self.invalidate_order();
    }

    /// Connect node output (`source`, `source_port`) to network output `global_output`.
    fn connect_output_index(
        &mut self,
        source: NodeIndex,
        source_port: PortIndex,
        global_output: PortIndex,
    ) {
        self.output_edge[global_output] = edge(
            Port::Local(source, source_port),
            Port::Global(global_output),
        );
        self.invalidate_order();
    }

    /// Pipe `source` outputs to global outputs.
    /// If there are fewer global outputs than `source` outputs, then only the first ones will be used.
    /// If there are more global outputs than `source` outputs,
    /// then a modulo operation is taken to obtain the `source` output.
    /// If there are no outputs in `source` at all, then zeros are supplied.
    ///
    /// ### Example (Stereo Reverb)
    /// ```
    /// use fundsp_tutti::prelude64::*;
    /// let mut net = Net::new(2, 2);
    /// let id = net.push(Box::new(multipass() & reverb_stereo(10.0, 1.0, 0.5)));
    /// net.pipe_input(id);
    /// net.pipe_output(id);
    /// net.check();
    /// ```
    pub fn pipe_output(&mut self, source: NodeId) {
        let source_index = self.node_index[&source];
        let node_outputs = self.vertex[source_index].outputs();
        for channel in 0..self.outputs() {
            if node_outputs > 0 {
                self.output_edge[channel] = edge(
                    Port::Local(source_index, channel % node_outputs),
                    Port::Global(channel),
                );
            } else {
                self.output_edge[channel] = edge(Port::Zero, Port::Global(channel));
            }
        }
        self.invalidate_order();
    }

    /// Resize the network's **global output** arity to `channels`, keeping any
    /// existing output edges for channels that remain and defaulting new
    /// channels to silence (`Zero`). Truncating drops the trailing edges.
    ///
    /// This exists so an **offline** graph (a clone driven by `tick`/`process`,
    /// e.g. surround export) can widen past the device's stereo output before
    /// (re-)`pipe_output`ing a wider master. It must **not** be called on a live
    /// network that has a backend taken (`has_backend()`), because the backend's
    /// channel count is fixed at `backend()` time and `commit()` asserts the two
    /// still agree; `Clone` clears the backend (`front: None`), which is exactly
    /// the offline case this serves.
    pub fn set_output_arity(&mut self, channels: usize) {
        let old = self.outputs();
        if channels == old {
            return;
        }
        debug_assert!(
            !self.has_backend(),
            "set_output_arity on a live-backed net would desync the CPAL backend; \
             use set_output_arity_live + commit_output_arity_change for the live path"
        );
        self.resize_output_edges(channels);
    }

    /// Grow/shrink the global output arity of a **live-backed** frontend, to be
    /// followed by [`commit_output_arity_change`](Self::commit_output_arity_change).
    ///
    /// Unlike [`set_output_arity`](Self::set_output_arity) this permits a taken
    /// backend — see `commit_output_arity_change` for the RT-buffer contract the
    /// caller must uphold. No-op if the arity is unchanged.
    pub fn set_output_arity_live(&mut self, channels: usize) {
        if channels == self.outputs() {
            return;
        }
        self.resize_output_edges(channels);
    }

    /// Resize `output`/`output_edge` to `channels`, normalizing each edge's
    /// `Global` target index. New channels default to silence (`Zero`).
    fn resize_output_edges(&mut self, channels: usize) {
        self.output.resize(channels);
        self.output_edge
            .resize_with(channels, || edge(Port::Zero, Port::Global(0)));
        // Normalize each edge's Global target index to its channel (resize_with
        // can't know the index; truncation leaves survivors correct).
        for (channel, e) in self.output_edge.iter_mut().enumerate() {
            e.target = Port::Global(channel);
        }
        self.invalidate_order();
    }

    /// Pass through global `input` to global `output`.
    ///
    /// ### Example (Stereo Pass-Through)
    /// ```
    /// use fundsp_tutti::prelude32::*;
    /// let mut net = Net::new(2, 2);
    /// net.pass_through(0, 0);
    /// net.pass_through(1, 1);
    /// net.check();
    /// ```
    pub fn pass_through(&mut self, input: PortIndex, output: PortIndex) {
        self.output_edge[output] = edge(Port::Global(input), Port::Global(output));
        self.invalidate_order();
    }

    /// Connect `source` node outputs to `target` node inputs.
    /// If there are more `source` outputs than `target` inputs, then some of the outputs will be unused.
    /// If there are fewer `source` outputs than `target` inputs, then a modulo operation is taken
    /// to obtain a `source` output.
    /// If there are no `source` outputs at all, then `target` inputs are filled with zeros.
    /// For example, mono output and stereo input results in the same output being sent to both channels.
    ///
    /// ### Example (Panned Sine Wave)
    /// ```
    /// use fundsp_tutti::prelude32::*;
    /// let mut net = Net::new(0, 2);
    /// let id1 = net.push(Box::new(sine_hz(440.0)));
    /// let id2 = net.push(Box::new(pan(0.0)));
    /// net.pipe_all(id1, id2);
    /// net.pipe_output(id2);
    /// net.check();
    /// ```
    pub fn pipe_all(&mut self, source: NodeId, target: NodeId) {
        // We should never panic here so just return if `source` is the same as `target`.
        if source == target {
            return;
        }
        let source_index = self.node_index[&source];
        let target_index = self.node_index[&target];
        let outputs = self.vertex[source_index].outputs();
        for channel in 0..self.vertex[target_index].inputs() {
            if outputs > 0 {
                self.vertex[target_index].source[channel] = edge(
                    Port::Local(source_index, channel % outputs),
                    Port::Local(target_index, channel),
                );
            } else {
                self.vertex[target_index].source[channel] =
                    edge(Port::Zero, Port::Local(target_index, channel));
            }
        }
        self.invalidate_order();
    }

    /// Number of nodes in the network.
    pub fn size(&self) -> usize {
        self.vertex.len()
    }

    /// Assuming this network is a chain of processing units,
    /// add a new unit to the end of the chain.
    /// Global outputs will be assigned to the outputs of the unit.
    /// If there are more global outputs than there are outputs in the unit, then a modulo
    /// is taken to plug all of them.
    /// If this is the first unit in the net, then global inputs are assigned to inputs of the unit;
    /// if the net was not empty then the previous global output sources become inputs to the unit.
    /// Returns the ID of the new unit.
    ///
    /// ### Example (Lowpass And Highpass Filters In Series)
    /// ```
    /// use fundsp_tutti::prelude32::*;
    /// let mut net = Net::new(1, 1);
    /// net.chain(Box::new(lowpass_hz(2000.0, 1.0)));
    /// net.chain(Box::new(highpass_hz(1000.0, 1.0)));
    /// net.check();
    /// ```
    pub fn chain(&mut self, unit: Box<dyn AudioUnit>) -> NodeId {
        let unit_inputs = unit.inputs();
        let id = self.push(unit);
        let index = self.node_index[&id];

        if self.size() == 1 {
            if self.inputs() > 0 {
                self.pipe_input(id);
            }
        } else {
            let global_outputs = self.outputs();
            for i in 0..unit_inputs {
                if global_outputs > 0 {
                    self.vertex[index].source[i].source =
                        self.output_edge[i % global_outputs].source;
                } else {
                    self.vertex[index].source[i].source = Port::Zero;
                }
            }
        }

        self.pipe_output(id);
        self.invalidate_order();
        id
    }

    /// Return whether the given `node` is contained in the network.
    pub fn contains(&self, node: NodeId) -> bool {
        self.node_index.contains_key(&node)
    }

    /// Return number of inputs in contained `node`.
    pub fn inputs_in(&self, node: NodeId) -> usize {
        self.vertex[self.node_index[&node]].inputs()
    }

    /// Return number of outputs in contained `node`.
    pub fn outputs_in(&self, node: NodeId) -> usize {
        self.vertex[self.node_index[&node]].outputs()
    }

    /// Access `node`. Note that if this network is a frontend,
    /// then the nodes accessible here are clones.
    pub fn node(&self, node: NodeId) -> &dyn AudioUnit {
        &*self.vertex[self.node_index[&node]].unit
    }

    /// Access mutable `node`. Note that any changes made via this method
    /// are not accounted in the backend. This can be used to, e.g.,
    /// query for frequency responses.
    pub fn node_mut(&mut self, node: NodeId) -> &mut dyn AudioUnit {
        &mut *self.vertex[self.node_index[&node]].unit
    }

    /// Typed read-access to `node`.
    ///
    /// Total where [`node`](Self::node) panics: returns `None` if `node` is not
    /// in the network (e.g. a stale id from a previous edit) or does not refer
    /// to a `T`.
    pub fn node_as<T: AudioUnit + 'static>(&self, node: NodeId) -> Option<&T> {
        let index = *self.node_index.get(&node)?;
        <dyn AudioUnit>::as_any(&*self.vertex[index].unit).downcast_ref::<T>()
    }

    /// Typed mutable access to `node`.
    ///
    /// Total where [`node_mut`](Self::node_mut) panics: returns `None` if
    /// `node` is not in the network or does not refer to a `T`.
    ///
    /// Carries the same caveat as `node_mut`: on a frontend the vertices are
    /// clones, so a mutation here reaches the audio thread only via the next
    /// [`commit`](Self::commit). Installing shared state (an `Arc` slot, a
    /// source handle) through this accessor is a silent no-op unless the state
    /// is itself shared — the clone discards anything stored by value.
    pub fn node_as_mut<T: AudioUnit + 'static>(&mut self, node: NodeId) -> Option<&mut T> {
        let index = *self.node_index.get(&node)?;
        <dyn AudioUnit>::as_any_mut(&mut *self.vertex[index].unit).downcast_mut::<T>()
    }

    /// Compute and store node order for this network.
    fn determine_order(&mut self) {
        // Update source vertex shortcut.
        for j in 0..self.vertex.len() {
            self.vertex[j].update_source_vertex();
        }
        // Update net hash. We have designed the hash to depend on vertices but not edges.
        let hash = self.ping(true, AttoHash::new(ID));
        self.ping(false, hash);
        let mut order = match self.order.take() {
            Some(v) => v,
            None => Vec::with_capacity(self.vertex.len()),
        };
        if self.determine_order_in(&mut order) {
            self.error = None;
        } else {
            self.error = Some(NetError::Cycle);
        }
        self.order = Some(order);
    }

    /// Determine node order in the supplied vector. Returns true if successful, false
    /// if a cycle was detected.
    fn determine_order_in(&mut self, order: &mut Vec<NodeIndex>) -> bool {
        // We calculate an inverse order here and then reverse it,
        // as that is efficient with the data we have at hand.
        // A feature of this algorithm is that vertices that are not
        // connected to outputs at all still get included in the ordering,
        // which is what we want: running nodes may have side effects.
        order.clear();

        for i in 0..self.vertex.len() {
            self.vertex[i].unplugged = 0;
            self.vertex[i].ordered = false;
        }

        for i in 0..self.vertex.len() {
            for channel in 0..self.vertex[i].inputs() {
                if let Port::Local(j, _) = self.vertex[i].source[channel].source {
                    self.vertex[j].unplugged += 1;
                }
            }
        }

        fn propagate(net: &mut Net, i: usize, order: &mut Vec<NodeIndex>) {
            for channel in 0..net.vertex[i].inputs() {
                if let Port::Local(j, _) = net.vertex[i].source[channel].source {
                    net.vertex[j].unplugged -= 1;
                    if net.vertex[j].unplugged == 0 {
                        net.vertex[j].ordered = true;
                        order.push(j);
                        propagate(net, j, order);
                    }
                }
            }
        }

        for i in 0..self.vertex.len() {
            if self.vertex[i].ordered {
                continue;
            }
            if self.vertex[i].unplugged == 0 {
                self.vertex[i].ordered = true;
                order.push(i);
                propagate(self, i, order);
            }
        }

        if order.len() < self.vertex.len() {
            // We missed some nodes, which means there must be one or more cycles.
            // Add the rest of the nodes anyway. The worst that can happen
            // is that we use old buffer data.
            for i in 0..self.vertex.len() {
                if !self.vertex[i].ordered {
                    order.push(i);
                }
            }
            order.reverse();
            false
        } else {
            order.reverse();
            true
        }
    }

    /// Wrap arbitrary unit in a network.
    ///
    /// ### Example (Conditional Processing)
    /// ```
    /// use fundsp_tutti::prelude64::*;
    /// let mut net = Net::wrap(Box::new(square_hz(440.0)));
    /// let add_filter = true;
    /// if add_filter {
    ///     net = net >> lowpass_hz(880.0, 1.0);
    /// }
    /// ```
    pub fn wrap(unit: Box<dyn AudioUnit>) -> Net {
        let mut net = Net::new(unit.inputs(), unit.outputs());
        let id = net.push(unit);
        if net.inputs() > 0 {
            net.pipe_input(id);
        }
        if net.outputs() > 0 {
            net.pipe_output(id);
        }
        net
    }

    /// Wrap arbitrary `unit` in a network. Return network and the ID of the unit.
    pub fn wrap_id(unit: Box<dyn AudioUnit>) -> (Net, NodeId) {
        let mut net = Net::new(unit.inputs(), unit.outputs());
        let id = net.push(unit);
        if net.inputs() > 0 {
            net.pipe_input(id);
        }
        if net.outputs() > 0 {
            net.pipe_output(id);
        }
        (net, id)
    }

    /// Create a network that outputs a scalar value on all channels.
    ///
    /// ### Example
    /// ```
    /// use fundsp_tutti::prelude32::*;
    /// let mut net = Net::scalar(2, 1.0);
    /// assert!(net.get_stereo() == (1.0, 1.0));
    /// ```
    pub fn scalar(channels: usize, scalar: f32) -> Net {
        let mut net = Net::new(0, channels);
        let id = net.push(Box::new(An(Constant::new([scalar].into()))));
        for i in 0..channels {
            net.connect_output(id, 0, i);
        }
        net
    }

    /// Check internal consistency of the network. Panic if something is wrong.
    pub fn check(&self) {
        assert_eq!(self.input.channels(), self.inputs());
        assert_eq!(self.output.channels(), self.outputs());
        assert_eq!(self.output_edge.len(), self.outputs());
        assert_eq!(self.node_index.len(), self.size());
        for channel in 0..self.outputs() {
            assert_eq!(self.output_edge[channel].target, Port::Global(channel));
            match self.output_edge[channel].source {
                Port::Local(node, port) => {
                    assert!(node < self.size());
                    assert!(port < self.vertex[node].outputs());
                }
                Port::Global(port) => {
                    assert!(port < self.inputs());
                }
                _ => (),
            }
        }
        for index in 0..self.size() {
            assert_eq!(self.node_index[&self.vertex[index].id], index);
            assert_eq!(self.vertex[index].source.len(), self.vertex[index].inputs());
            assert_eq!(
                self.vertex[index].input.channels(),
                self.vertex[index].inputs()
            );
            assert_eq!(
                self.vertex[index].output.channels(),
                self.vertex[index].outputs()
            );
            assert_eq!(
                self.vertex[index].tick_input.len(),
                self.vertex[index].inputs()
            );
            assert_eq!(
                self.vertex[index].tick_output.len(),
                self.vertex[index].outputs()
            );
            for channel in 0..self.vertex[index].inputs() {
                assert_eq!(
                    self.vertex[index].source[channel].target,
                    Port::Local(index, channel)
                );
                match self.vertex[index].source[channel].source {
                    Port::Local(node, port) => {
                        assert!(node < self.size());
                        // Self connections are prohibited.
                        assert!(node != index);
                        assert!(port < self.vertex[node].outputs());
                    }
                    Port::Global(port) => {
                        assert!(port < self.inputs());
                    }
                    _ => (),
                }
            }
            if let Some((source_node, source_port)) = self.vertex[index].source_vertex {
                assert!(source_node < self.size());
                assert!(source_node != index);
                assert!(
                    source_port + self.vertex[index].inputs() <= self.vertex[source_node].outputs()
                );
            }
        }
    }

    /// Disambiguate IDs in this network so they don't conflict with those in `other` network.
    /// Conflict is possible as a result of cloning and recombination.
    fn disambiguate_ids(&mut self, other: &Net) {
        for i in 0..self.size() {
            let id = self.vertex[i].id;
            if other.node_index.contains_key(&id) {
                self.node_index.remove(&id);
                let new_id = NodeId::new();
                self.vertex[i].id = new_id;
                self.node_index.insert(new_id, i);
            }
        }
    }

    /// Migrate existing units to the new network. This is an internal function.
    /// Carry the backend's live units across into an incoming network.
    ///
    /// **This is where a by-value control edit is discarded**, and the loss is
    /// silent — worth stating here because the symptom (a fader that moves on
    /// screen and not in the sound) appears nowhere near this function.
    ///
    /// The swap below keeps the *backend's* unit whenever the incoming vertex
    /// reports no change since the last update. A caller that mutated a
    /// frontend clone through [`node_as_mut`](Self::node_as_mut) — storing into
    /// a plain field rather than through an `Arc` — has produced exactly such a
    /// vertex, so its edit is thrown away rather than merely deferred.
    ///
    /// The rule that avoids it lives in `tutti_nodes`' crate docs: a live
    /// control value belongs behind an `Arc` (`Param<U>`, `Arc<AtomicBool>`),
    /// where both copies see one cell and this swap is harmless.
    pub(crate) fn migrate(&mut self, new: &mut Net) {
        // Carry the unaddressed counter into the incoming network, because the
        // caller is about to `mem::swap` it into place as the live backend.
        // `new` arrived from the frontend through `Clone`, which deliberately
        // gives every clone a *fresh* cell — correct for an ordinary clone (a
        // separate graph) and wrong for this one, which is about to *become* this
        // backend. Without this line the counter is orphaned on the first commit:
        // the audio thread keeps counting into a cell no host still holds, and
        // `take_unaddressed_settings` reads zero forever.
        new.unaddressed_settings = self.unaddressed_settings.clone();
        for (id, &index) in self.node_index.iter() {
            if let Some(&new_index) = new.node_index.get(id) {
                // We may use the existing unit if no changes have been made since our last update.
                // Note: the new vertices never contain next or latest units as they come from the frontend
                // where they are not applied.
                if new.vertex[new_index].changed <= self.revision {
                    core::mem::swap(
                        &mut self.vertex[index].unit,
                        &mut new.vertex[new_index].unit,
                    );
                    core::mem::swap(
                        &mut self.vertex[index].next,
                        &mut new.vertex[new_index].next,
                    );
                    core::mem::swap(
                        &mut self.vertex[index].latest,
                        &mut new.vertex[new_index].latest,
                    );
                    new.vertex[new_index].fade_phase = self.vertex[index].fade_phase;
                }
            }
        }
    }

    /// Create a real-time friendly backend for this network.
    /// This network is then the frontend and any changes made can be committed to the backend.
    /// The backend is initialized with the current state of the network.
    /// This can be called only once for a network.
    ///
    /// ### Example
    /// ```
    /// use fundsp_tutti::prelude64::*;
    /// let mut net = Net::new(0, 1);
    /// net.chain(Box::new(dc(1.0)));
    /// let mut backend = net.backend();
    /// net.chain(Box::new(mul(2.0)));
    /// assert!(backend.get_mono() == 1.0);
    /// net.commit();
    /// assert!(backend.get_mono() == 2.0);
    /// ```
    pub fn backend(&mut self) -> NetBackend {
        assert!(!self.has_backend());
        // Create huge channel buffers to make sure we don't run out of space easily.
        let queue_message = Arc::new(Queue::<NetMessage, 256>::new_const());
        let queue_return = Arc::new(Queue::<NetReturn, 256>::new_const());
        self.front = Some((queue_message.clone(), queue_return.clone()));
        self.backend_inputs = self.inputs();
        self.backend_outputs = self.outputs();
        if !self.is_ordered() {
            self.determine_order();
        }
        let mut net = self.clone();
        // Send over the original nodes to the backend.
        // This is necessary if the nodes contain any backends, which cannot be cloned effectively.
        core::mem::swap(&mut net.vertex, &mut self.vertex);
        // Share the unaddressed counter, rather than let the clone keep its own.
        // The backend is where a misaddressed setting is actually detected — it
        // resolves against `node_index` while the frontend only enqueues — so
        // without this the count would accumulate on the audio thread where no
        // host can read it. `Clone` deliberately does *not* share (a cloned
        // network is a separate graph); this is the one seam where the two halves
        // are the same network and must agree.
        net.unaddressed_settings = self.unaddressed_settings.clone();
        net.allocate();
        self.revision += 1;
        NetBackend::new(queue_return.clone(), queue_message.clone(), net)
    }

    /// Returns whether this network has a backend.
    pub fn has_backend(&self) -> bool {
        self.front.is_some()
    }

    /// Commit changes made to this frontend to the backend.
    /// This may be called only if the network has a backend.
    pub fn commit(&mut self) {
        self.commit_inner(false);
    }

    /// EXPERIMENTAL: commit, permitting the global **output** arity to change.
    ///
    /// The standard [`commit`](Self::commit) panics if `outputs()` differs from
    /// what it was when [`backend`](Self::backend) was taken, because the usual
    /// RT driver allocates its output buffer once from `outputs()` and reuses it
    /// — a changed arity would desync that buffer. This variant instead accepts
    /// the new arity and updates the recorded `backend_outputs`, shipping the
    /// wider/narrower net to the backend like any other commit.
    ///
    /// Contract: the RT `process`/`tick` caller MUST size the output buffer from
    /// `NetBackend::outputs()` **re-read each block**, not from a value cached at
    /// stream-build time. A caller that renders into a fresh, arity-sliced
    /// scratch each block (e.g. `tutti_core::Engine`) satisfies this.
    pub fn commit_output_arity_change(&mut self) {
        self.commit_inner(true);
    }

    fn commit_inner(&mut self, allow_output_arity_change: bool) {
        assert!(self.has_backend());
        if self.inputs() != self.backend_inputs {
            panic!(
                "The number of inputs has changed since last commit. The number of inputs must stay the same."
            );
        }
        if self.outputs() != self.backend_outputs {
            if allow_output_arity_change {
                self.backend_outputs = self.outputs();
            } else {
                panic!(
                    "The number of outputs has changed since last commit. The number of outputs must stay the same."
                );
            }
        }
        if !self.is_ordered() {
            self.determine_order();
        }
        let mut net = self.clone();
        // Filter the edit queue while updating unit indices.
        for edit in self.edit_queue.iter_mut() {
            if let Some(&index) = self.node_index.get(&edit.id) {
                net.edit_queue.push(NodeEdit {
                    unit: edit.unit.take(),
                    id: edit.id,
                    index,
                    fade: edit.fade.clone(),
                    fade_time: edit.fade_time,
                });
            }
        }
        self.edit_queue.clear();
        // Send over the original nodes to the backend.
        // This is necessary if the nodes contain any backends, which cannot be cloned effectively.
        core::mem::swap(&mut net.vertex, &mut self.vertex);
        // Preallocate all necessary memory.
        net.allocate();
        if let Some((sender, receiver)) = &mut self.front {
            // Deallocate all previous versions.
            loop {
                match receiver.dequeue() {
                    Some(NetReturn::Null) => (),
                    Some(NetReturn::Net(net)) => drop(net),
                    Some(NetReturn::Unit(unit)) => drop(unit),
                    _ => break,
                }
            }
            // Send the new version over.
            if sender.enqueue(NetMessage::Net(Box::new(net))).is_ok() {}
        }
        self.revision += 1;
    }

    /// Resolve new frontend for a binary combination.
    fn resolve_frontend(&mut self, other: &mut Net) {
        if self.has_backend() && other.has_backend() {
            panic!("Cannot combine two frontends.");
        }
        if other.has_backend() {
            core::mem::swap(&mut self.front, &mut other.front);
            core::mem::swap(&mut self.edit_queue, &mut other.edit_queue);
            self.backend_inputs = other.backend_inputs;
            self.backend_outputs = other.backend_outputs;
            self.revision = other.revision;
        }
    }

    /// Process one sample using the supplied `sender` to deallocate units.
    #[inline]
    pub(crate) fn tick_2(
        &mut self,
        input: &[f32],
        output: &mut [f32],
        sender: &Option<Arc<Queue<NetReturn, 256>>>,
    ) {
        if !self.is_ordered() {
            self.determine_order();
        }
        // Iterate units in network order.
        for &node_index in self.order.get_or_insert(Vec::new()).iter() {
            for channel in 0..self.vertex[node_index].inputs() {
                match self.vertex[node_index].source[channel].source {
                    Port::Zero => self.vertex[node_index].tick_input[channel] = 0.0,
                    Port::Global(port) => self.vertex[node_index].tick_input[channel] = input[port],
                    Port::Local(source, port) => {
                        self.vertex[node_index].tick_input[channel] =
                            self.vertex[source].tick_output[port]
                    }
                }
            }
            let sample_rate_f32 = self.sample_rate as f32;
            let vertex = &mut self.vertex[node_index];
            vertex.tick(sample_rate_f32, sender);
        }

        // Then we set the global outputs.
        for channel in 0..output.len() {
            match self.output_edge[channel].source {
                Port::Global(port) => output[channel] = input[port],
                Port::Local(node, port) => output[channel] = self.vertex[node].tick_output[port],
                Port::Zero => output[channel] = 0.0,
            }
        }
    }

    /// Process a block of samples using the supplied `sender` to deallocate units.
    #[inline]
    pub(crate) fn process_2(
        &mut self,
        size: usize,
        input: &BufferRef,
        output: &mut BufferMut,
        sender: &Option<Arc<Queue<NetReturn, 256>>>,
    ) {
        if !self.is_ordered() {
            self.determine_order();
        }
        let simd_size = simd_items(size);
        // Iterate units in network order.
        for &node_index in self.order.as_ref().unwrap().iter() {
            if let Some((source_node, source_port)) = self.vertex[node_index].source_vertex {
                // We can source inputs directly from a source vertex.
                let ptr = &mut self.vertex[source_node].output as *mut BufferVec;
                let vertex = &mut self.vertex[node_index];
                // Safety: we know there is no aliasing, as self connections are prohibited.
                let sample_rate_f32 = self.sample_rate as f32;
                unsafe {
                    vertex.process(
                        size,
                        &(*ptr).buffer_ref().subset(source_port, vertex.inputs()),
                        sample_rate_f32,
                        sender,
                    );
                }
            } else {
                let ptr = &mut self.vertex[node_index].input as *mut BufferVec;
                // Gather inputs for this vertex.
                for channel in 0..self.vertex[node_index].inputs() {
                    // Safety: we know there is no aliasing, as self connections are prohibited.
                    unsafe {
                        match self.vertex[node_index].source[channel].source {
                            Port::Zero => (*ptr).channel_mut(channel)[..simd_size].fill(F32x::ZERO),
                            Port::Global(port) => (*ptr).channel_mut(channel)[..simd_size]
                                .copy_from_slice(&input.channel(port)[..simd_size]),
                            Port::Local(source, port) => {
                                (*ptr).channel_mut(channel)[..simd_size].copy_from_slice(
                                    &self.vertex[source].output.channel(port)[..simd_size],
                                );
                            }
                        }
                    }
                }
                let sample_rate_f32 = self.sample_rate as f32;
                let vertex = &mut self.vertex[node_index];
                // Safety: we know there is no aliasing, as self connections are prohibited.
                unsafe {
                    vertex.process(size, &(*ptr).buffer_ref(), sample_rate_f32, sender);
                }
            }
        }

        // Then we set the global outputs.
        for channel in 0..output.channels() {
            match self.output_edge[channel].source {
                Port::Global(port) => output.channel_mut(channel)[..simd_size]
                    .copy_from_slice(&input.channel(port)[..simd_size]),
                Port::Local(node, port) => output.channel_mut(channel)[..simd_size]
                    .copy_from_slice(&self.vertex[node].output.channel(port)[..simd_size]),
                Port::Zero => output.channel_mut(channel)[..simd_size].fill(F32x::ZERO),
            }
        }
    }

    /// Apply all edits into this network.
    pub(crate) fn apply_edits(&mut self, sender: &Option<Arc<Queue<NetReturn, 256>>>) {
        for edit in self.edit_queue.iter_mut() {
            self.vertex[edit.index].enqueue(edit, sender);
        }
        self.edit_queue.clear();
    }

    /// Apply all edits from another network into this network.
    pub(crate) fn apply_foreign_edits(
        &mut self,
        source: &mut Net,
        sender: &Option<Arc<Queue<NetReturn, 256>>>,
    ) {
        for edit in source.edit_queue.iter_mut() {
            if let Some(index) = self.node_index.get(&edit.id) {
                self.vertex[*index].enqueue(edit, sender);
            }
        }
    }
}

impl AudioUnit for Net {
    fn inputs(&self) -> usize {
        self.input.channels()
    }

    fn outputs(&self) -> usize {
        self.output.channels()
    }

    fn set_sample_rate(&mut self, sample_rate: crate::SampleRate) {
        let sample_rate_raw: f64 = sample_rate.get();
        if self.sample_rate != sample_rate_raw {
            self.sample_rate = sample_rate_raw;
            for vertex in &mut self.vertex {
                vertex.unit.set_sample_rate(sample_rate);
                // Sample rate change counts as a change
                // to be sent to the backend because
                // we cannot change sample rate in the backend
                // - it may allocate or do something else inappropriate.
                vertex.changed = self.revision;
            }
            // Take the opportunity to unload some calculations.
            if !self.is_ordered() {
                self.determine_order();
            }
        }
    }

    fn reset(&mut self) {
        for vertex in &mut self.vertex {
            vertex.unit.reset();
            // Reseting a unit counts as a change
            // to be sent to the backend because
            // we cannot reset in the backend
            // - it may allocate or do something else inappropriate.
            vertex.changed = self.revision;
        }
        // Take the opportunity to unload some calculations.
        if !self.is_ordered() {
            self.determine_order();
        }
    }

    /// Forwarded to every vertex, so a nested network is severed too.
    ///
    /// Without this, a `Net` used as a node inherits the do-nothing default and
    /// its children are never reached — anything sharing live state one level
    /// down stays attached to what the audio thread is reading, which is the
    /// hazard `isolate` exists to remove.
    fn isolate(&mut self) {
        for vertex in &mut self.vertex {
            vertex.unit.isolate();
            vertex.changed = self.revision;
        }
    }

    /// Forwarded to every vertex, for the same reason as [`isolate`](Self::isolate).
    ///
    /// A transport-aware node inside a sub-network would otherwise keep the live
    /// transport and render against a playhead nothing advances — silence, with
    /// nothing to compare and no error to raise.
    fn rebind_offline(&mut self, ctx: &dyn core::any::Any) {
        for vertex in &mut self.vertex {
            vertex.unit.rebind_offline(ctx);
            vertex.changed = self.revision;
        }
    }

    /// Forkable only if every vertex is: a sub-network answers for the
    /// units inside it, as it forwards `isolate` to them.
    fn forkable(&self) -> bool {
        self.vertex.iter().all(|v| v.unit.forkable())
    }

    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        self.tick_2(input, output, &None);
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        self.process_2(size, input, output, &None);
    }

    /// Apply a setting, or count it as lost.
    ///
    /// **Every path a setting can vanish down is counted.** It used to have an
    /// `if`/`else if` with no `else`: a setting naming a node this network does
    /// not contain was discarded in silence, and `dropped_settings` counted only
    /// the queue-full case — so the counter read zero while parameter writes
    /// disappeared. That is the top of the silent-no-op stack that
    /// `tutti-nodes`' module docs describe, and it made a wrong `NodeId` present
    /// as "the fader moves on screen and not in the sound", indistinguishable
    /// from a unit that ignores the param.
    ///
    /// The two counters answer different questions and must not be merged:
    /// [`dropped_settings`](Self::take_dropped_settings) is backpressure and
    /// clears itself, [`unaddressed_settings`](Self::take_unaddressed_settings)
    /// is a wiring bug that will lose every future write to the same target.
    fn set(&mut self, setting: Setting) {
        if let Some((sender, _receiver)) = &mut self.front {
            // A frontend does not resolve the address — the backend owns the
            // authoritative `node_index`, and this network's copy may be a
            // revision behind. So this arm can only report queue-full; a bad
            // address is caught on the other side.
            if sender.enqueue(NetMessage::Setting(setting)).is_err() {
                self.dropped_settings += 1;
            }
            return;
        }
        if let Address::Node(addr) = setting.direction()
            && let Some(index) = self.node_index.get(&NodeId::from(addr))
        {
            self.vertex[*index].unit.set(setting.peel());
            return;
        }
        // Fell through: either the address is not a node at all, or it names one
        // this network does not contain. Both are unroutable here and neither
        // recovers on its own.
        self.unaddressed_settings.fetch_add(1, Ordering::Relaxed);
    }

    fn get_id(&self) -> u64 {
        ID
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn ping(&mut self, probe: bool, hash: AttoHash) -> AttoHash {
        let mut hash = hash.hash(ID);
        for x in self.vertex.iter_mut() {
            hash = x.unit.ping(probe, hash);
        }
        hash
    }

    fn route(&mut self, input: &SignalFrame, frequency: f64) -> SignalFrame {
        let mut inner_signal: Vec<SignalFrame> = Vec::new();
        for vertex in self.vertex.iter() {
            inner_signal.push(SignalFrame::new(vertex.unit.outputs()));
        }
        if !self.is_ordered() {
            self.determine_order();
        }
        for &unit_index in self.order.as_mut().unwrap().iter() {
            let mut input_signal = SignalFrame::new(self.vertex[unit_index].unit.inputs());
            for channel in 0..self.vertex[unit_index].unit.inputs() {
                match self.vertex[unit_index].source[channel].source {
                    Port::Local(j, port) => input_signal.set(channel, inner_signal[j].at(port)),
                    Port::Global(j) => input_signal.set(channel, input.at(j)),
                    Port::Zero => input_signal.set(channel, Signal::Value(0.0)),
                }
            }
            inner_signal[unit_index] = self.vertex[unit_index].unit.route(&input_signal, frequency);
        }

        // Then we set the global outputs.
        let mut output_signal = SignalFrame::new(self.outputs());
        for channel in 0..self.outputs() {
            match self.output_edge[channel].source {
                Port::Global(port) => output_signal.set(channel, input.at(port)),
                Port::Local(node, port) => {
                    output_signal.set(channel, inner_signal[node].at(port));
                }
                Port::Zero => output_signal.set(channel, Signal::Value(0.0)),
            }
        }
        output_signal
    }

    /// A net's tail is the tail of the graph it contains.
    ///
    /// Without this a `Net` pushed as a node inside another would report the
    /// [`Tail::Unknown`] default, so a plugin one level down would be invisible
    /// to the outer walk. Recursing here is what makes nesting transparent —
    /// [`tutti_types::tail::graph_tail`] reaches every node at this level, and
    /// each node that is itself a net answers for its own.
    ///
    /// An unbounded or partly-unknown inner graph collapses to that arm: the
    /// outer walk composes frame counts, and neither of those is one.
    fn tail(&mut self) -> Tail {
        let inner = tutti_types::tail::graph_tail(&*self);
        if inner.is_unbounded() {
            return Tail::Unbounded;
        }
        match inner.samples() {
            Some(s) if s.is_zero() => Tail::None,
            Some(s) => Tail::Finite(s),
            None => Tail::Unknown,
        }
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
    }

    fn allocate(&mut self) {
        if !self.is_ordered() {
            self.determine_order();
        }
        for vertex in self.vertex.iter_mut() {
            vertex.allocate();
        }
    }
}

impl Net {
    /// Return whether `Net::thru(net)` is valid. This returns true always.
    #[allow(unused_variables)]
    pub fn can_thru(net: &Net) -> bool {
        true
    }

    /// Given `net`, create and return network `!net`.
    pub fn thru(mut net: Net) -> Net {
        let outputs = net.outputs();
        net.output.resize(net.inputs());
        net.output_edge
            .resize(net.inputs(), edge(Port::Zero, Port::Zero));
        for i in outputs..net.inputs() {
            net.output_edge[i] = edge(Port::Global(i), Port::Global(i));
        }
        net.invalidate_order();
        net
    }

    /// Return whether `Net::branch(net1, net2)` is valid.
    pub fn can_branch(net1: &Net, net2: &Net) -> bool {
        net1.inputs() == net2.inputs()
    }

    /// Given nets `net1` and `net2`, create and return net `net1 ^ net2`.
    pub fn branch(mut net1: Net, mut net2: Net) -> Net {
        if net1.inputs() != net2.inputs() {
            panic!(
                "Net::branch: mismatched inputs ({} versus {}).",
                net1.inputs(),
                net2.inputs()
            );
        }
        net2.disambiguate_ids(&net1);
        let offset = net1.vertex.len();
        let output_offset = net1.outputs();
        let outputs = net1.outputs() + net2.outputs();
        net1.vertex.append(&mut net2.vertex);
        net1.output_edge.append(&mut net2.output_edge);
        net1.output.resize(outputs);
        for i in output_offset..net1.output_edge.len() {
            match net1.output_edge[i].source {
                Port::Local(source_node, source_port) => {
                    net1.output_edge[i] = edge(
                        Port::Local(source_node + offset, source_port),
                        Port::Global(i),
                    );
                }
                Port::Global(source_port) => {
                    net1.output_edge[i] = edge(Port::Global(source_port), Port::Global(i));
                }
                Port::Zero => {
                    net1.output_edge[i] = edge(Port::Zero, Port::Global(i));
                }
            }
        }
        for node in offset..net1.vertex.len() {
            net1.node_index.insert(net1.vertex[node].id, node);
            for port in 0..net1.vertex[node].inputs() {
                match net1.vertex[node].source[port].source {
                    Port::Local(source_node, source_port) => {
                        net1.vertex[node].source[port] = edge(
                            Port::Local(source_node + offset, source_port),
                            Port::Local(node, port),
                        );
                    }
                    Port::Global(source_port) => {
                        net1.vertex[node].source[port] =
                            edge(Port::Global(source_port), Port::Local(node, port));
                    }
                    Port::Zero => {
                        net1.vertex[node].source[port] = edge(Port::Zero, Port::Local(node, port));
                    }
                }
            }
        }
        net1.invalidate_order();
        net1.resolve_frontend(&mut net2);
        net1
    }

    /// Return whether `Net::stack(net1, net2)` is valid. This returns true always.
    #[allow(unused_variables)]
    pub fn can_stack(net1: &Net, net2: &Net) -> bool {
        true
    }

    /// Given nets `net1` and `net2`, create and return net `net1 | net2`.
    pub fn stack(mut net1: Net, mut net2: Net) -> Net {
        net2.disambiguate_ids(&net1);
        let offset = net1.vertex.len();
        let output_offset = net1.outputs();
        let input_offset = net1.inputs();
        let inputs = net1.inputs() + net2.inputs();
        let outputs = net1.outputs() + net2.outputs();
        net1.vertex.append(&mut net2.vertex);
        net1.output_edge.append(&mut net2.output_edge);
        net1.output.resize(outputs);
        net1.input.resize(inputs);
        for i in output_offset..net1.output_edge.len() {
            match net1.output_edge[i].source {
                Port::Local(source_node, source_port) => {
                    net1.output_edge[i] = edge(
                        Port::Local(source_node + offset, source_port),
                        Port::Global(i),
                    );
                }
                Port::Global(source_port) => {
                    net1.output_edge[i] =
                        edge(Port::Global(source_port + input_offset), Port::Global(i));
                }
                Port::Zero => {
                    net1.output_edge[i] = edge(Port::Zero, Port::Global(i));
                }
            }
        }
        for node in offset..net1.vertex.len() {
            net1.node_index.insert(net1.vertex[node].id, node);
            for port in 0..net1.vertex[node].inputs() {
                match net1.vertex[node].source[port].source {
                    Port::Local(source_node, source_port) => {
                        net1.vertex[node].source[port] = edge(
                            Port::Local(source_node + offset, source_port),
                            Port::Local(node, port),
                        );
                    }
                    Port::Global(source_port) => {
                        net1.vertex[node].source[port] = edge(
                            Port::Global(source_port + input_offset),
                            Port::Local(node, port),
                        );
                    }
                    Port::Zero => {
                        net1.vertex[node].source[port] = edge(Port::Zero, Port::Local(node, port));
                    }
                }
            }
        }
        net1.invalidate_order();
        net1.resolve_frontend(&mut net2);
        net1
    }

    /// Return whether `Net::binary(net1, net2, ...)` is valid.
    pub fn can_binary(net1: &Net, net2: &Net) -> bool {
        net1.outputs() == net2.outputs()
    }

    /// Given nets `net1` and `net2` and binary operator `op`, create and return network `net1 op net2`.
    pub fn binary<B: FrameBinop<super::typenum::U1> + Sync + Send + 'static>(
        mut net1: Net,
        mut net2: Net,
        op: B,
    ) -> Net {
        if net1.outputs() != net2.outputs() {
            panic!(
                "Net::binary: mismatched outputs ({} versus {}).",
                net1.outputs(),
                net2.outputs()
            );
        }
        net2.disambiguate_ids(&net1);
        let output1 = net1.output_edge.clone();
        let output2 = net2.output_edge.clone();
        let input_offset = net1.inputs();
        let inputs = net1.inputs() + net2.inputs();
        let offset = net1.vertex.len();
        net1.vertex.append(&mut net2.vertex);
        net1.input.resize(inputs);
        for node in offset..net1.vertex.len() {
            net1.node_index.insert(net1.vertex[node].id, node);
            for port in 0..net1.vertex[node].inputs() {
                match net1.vertex[node].source[port].source {
                    Port::Local(source_node, source_port) => {
                        net1.vertex[node].source[port] = edge(
                            Port::Local(source_node + offset, source_port),
                            Port::Local(node, port),
                        );
                    }
                    Port::Global(source_port) => {
                        net1.vertex[node].source[port] = edge(
                            Port::Global(source_port + input_offset),
                            Port::Local(node, port),
                        );
                    }
                    Port::Zero => {
                        net1.vertex[node].source[port] = edge(Port::Zero, Port::Local(node, port));
                    }
                }
            }
        }
        let add_offset = net1.vertex.len();
        for i in 0..net1.outputs() {
            net1.push(Box::new(An(Binop::<_, _, _>::new(
                op.clone(),
                Pass::new(),
                Pass::new(),
            ))));
            net1.connect_output_index(add_offset + i, 0, i);
        }
        for i in 0..output1.len() {
            match output1[i].source {
                Port::Local(source_node, source_port) => {
                    net1.connect_index(source_node, source_port, add_offset + i, 0);
                }
                Port::Global(source_port) => {
                    net1.connect_input_index(source_port, add_offset + i, 0);
                }
                _ => (),
            }
        }
        for i in 0..output2.len() {
            match output2[i].source {
                Port::Local(source_node, source_port) => {
                    net1.connect_index(source_node + offset, source_port, add_offset + i, 1);
                }
                Port::Global(source_port) => {
                    net1.connect_input_index(source_port + input_offset, add_offset + i, 1);
                }
                _ => (),
            }
        }
        net1.invalidate_order();
        net1.resolve_frontend(&mut net2);
        net1
    }

    /// Return whether `Net::sum(net1, net2)` is valid.
    pub fn can_sum(net1: &Net, net2: &Net) -> bool {
        Net::can_binary(net1, net2)
    }

    /// Given nets `net1` and `net2`, create and return net `net1 + net2`.
    pub fn sum(net1: Net, net2: Net) -> Net {
        Net::binary(net1, net2, FrameAdd::new())
    }

    /// Return whether `Net::sum(net1, net2)` is valid.
    pub fn can_product(net1: &Net, net2: &Net) -> bool {
        Net::can_binary(net1, net2)
    }

    /// Given nets `net1` and `net2`, create and return net `net1 * net2`.
    pub fn product(net1: Net, net2: Net) -> Net {
        Net::binary(net1, net2, FrameMul::new())
    }

    /// Return whether `Net::bus(net1, net2)` is valid.
    pub fn can_bus(net1: &Net, net2: &Net) -> bool {
        net1.inputs() == net2.inputs() && net1.outputs() == net2.outputs()
    }

    /// Given nets `net1` and `net2`, create and return network `net1 & net2`.
    pub fn bus(mut net1: Net, mut net2: Net) -> Net {
        if net1.inputs() != net2.inputs() {
            panic!(
                "Net::bus: mismatched inputs ({} versus {}).",
                net1.inputs(),
                net2.inputs()
            );
        }
        if net1.outputs() != net2.outputs() {
            panic!(
                "Net::bus: mismatched outputs ({} versus {}).",
                net1.outputs(),
                net2.outputs()
            );
        }
        net2.disambiguate_ids(&net1);
        let output1 = net1.output_edge.clone();
        let output2 = net2.output_edge.clone();
        let offset = net1.vertex.len();
        net1.vertex.append(&mut net2.vertex);
        for node in offset..net1.vertex.len() {
            net1.node_index.insert(net1.vertex[node].id, node);
            for port in 0..net1.vertex[node].inputs() {
                match net1.vertex[node].source[port].source {
                    Port::Local(source_node, source_port) => {
                        net1.vertex[node].source[port] = edge(
                            Port::Local(source_node + offset, source_port),
                            Port::Local(node, port),
                        );
                    }
                    Port::Global(source_port) => {
                        net1.vertex[node].source[port] =
                            edge(Port::Global(source_port), Port::Local(node, port));
                    }
                    Port::Zero => {
                        net1.vertex[node].source[port] = edge(Port::Zero, Port::Local(node, port));
                    }
                }
            }
        }
        let add_offset = net1.vertex.len();
        for i in 0..net1.outputs() {
            net1.push(Box::new(An(Binop::<_, _, _>::new(
                FrameAdd::new(),
                Pass::new(),
                Pass::new(),
            ))));
            net1.connect_output_index(add_offset + i, 0, i);
        }
        for i in 0..output1.len() {
            match output1[i].source {
                Port::Local(source_node, source_port) => {
                    net1.connect_index(source_node, source_port, add_offset + i, 0);
                }
                Port::Global(source_port) => {
                    net1.connect_input_index(source_port, add_offset + i, 0);
                }
                _ => (),
            }
        }
        for i in 0..output2.len() {
            match output2[i].source {
                Port::Local(source_node, source_port) => {
                    net1.connect_index(source_node + offset, source_port, add_offset + i, 1);
                }
                Port::Global(source_port) => {
                    net1.connect_input_index(source_port, add_offset + i, 1);
                }
                _ => (),
            }
        }
        net1.invalidate_order();
        net1.resolve_frontend(&mut net2);
        net1
    }

    /// Return whether `Net::pipe(net1, net2)` is valid.
    pub fn can_pipe(net1: &Net, net2: &Net) -> bool {
        net1.outputs() == net2.inputs()
    }

    /// Given nets `net1` and `net2`, create and return net `net1 >> net2`.
    pub fn pipe(mut net1: Net, mut net2: Net) -> Net {
        if net1.outputs() != net2.inputs() {
            panic!(
                "Net::pipe: mismatched connectivity ({} outputs versus {} inputs).",
                net1.outputs(),
                net2.inputs()
            );
        }
        net2.disambiguate_ids(&net1);
        let offset = net1.vertex.len();
        net1.vertex.append(&mut net2.vertex);
        // Adjust local ports.
        for node in offset..net1.vertex.len() {
            net1.node_index.insert(net1.vertex[node].id, node);
            for port in 0..net1.vertex[node].inputs() {
                match net1.vertex[node].source[port].source {
                    Port::Local(source_node, source_port) => {
                        net1.vertex[node].source[port] = edge(
                            Port::Local(source_node + offset, source_port),
                            Port::Local(node, port),
                        );
                    }
                    Port::Global(source_port) => {
                        net1.vertex[node].source[port] = edge(
                            net1.output_edge[source_port].source,
                            Port::Local(node, port),
                        );
                    }
                    Port::Zero => {
                        net1.vertex[node].source[port] = edge(Port::Zero, Port::Local(node, port));
                    }
                }
            }
        }
        // Adjust output ports.
        let output_edge1 = net1.output_edge.clone();
        net1.output_edge.clone_from(&net2.output_edge);
        net1.output = net2.output.clone();
        for output_port in 0..net1.outputs() {
            match net1.output_edge[output_port].source {
                Port::Local(source_node, source_port) => {
                    net1.output_edge[output_port] = edge(
                        Port::Local(source_node + offset, source_port),
                        Port::Global(output_port),
                    );
                }
                Port::Global(source_port) => {
                    net1.output_edge[output_port] =
                        edge(output_edge1[source_port].source, Port::Global(output_port));
                }
                _ => (),
            }
        }
        net1.invalidate_order();
        net1.resolve_frontend(&mut net2);
        net1
    }
}

impl core::ops::Not for Net {
    type Output = Net;
    #[inline]
    fn not(self) -> Self::Output {
        Net::thru(self)
    }
}

impl core::ops::Neg for Net {
    type Output = Net;
    #[inline]
    fn neg(self) -> Self::Output {
        // TODO. Optimize this.
        let n = self.outputs();
        Net::scalar(n, f32::zero()) - self
    }
}

impl core::ops::Shr<Net> for Net {
    type Output = Net;
    #[inline]
    fn shr(self, y: Net) -> Self::Output {
        Net::pipe(self, y)
    }
}

impl<X> core::ops::Shr<An<X>> for Net
where
    X: AudioNode + core::marker::Send + Sync + 'static,
{
    type Output = Net;
    #[inline]
    fn shr(self, y: An<X>) -> Self::Output {
        Net::pipe(self, Net::wrap(Box::new(y)))
    }
}

impl<X> core::ops::Shr<Net> for An<X>
where
    X: AudioNode + core::marker::Send + Sync + 'static,
{
    type Output = Net;
    #[inline]
    fn shr(self, y: Net) -> Self::Output {
        Net::pipe(Net::wrap(Box::new(self)), y)
    }
}

impl core::ops::BitAnd<Net> for Net {
    type Output = Net;
    #[inline]
    fn bitand(self, y: Net) -> Self::Output {
        Net::bus(self, y)
    }
}

impl<X> core::ops::BitAnd<An<X>> for Net
where
    X: AudioNode + core::marker::Send + Sync + 'static,
{
    type Output = Net;
    #[inline]
    fn bitand(self, y: An<X>) -> Self::Output {
        Net::bus(self, Net::wrap(Box::new(y)))
    }
}

impl<X> core::ops::BitAnd<Net> for An<X>
where
    X: AudioNode + core::marker::Send + Sync + 'static,
{
    type Output = Net;
    #[inline]
    fn bitand(self, y: Net) -> Self::Output {
        Net::bus(Net::wrap(Box::new(self)), y)
    }
}

impl core::ops::BitOr<Net> for Net {
    type Output = Net;
    #[inline]
    fn bitor(self, y: Net) -> Self::Output {
        Net::stack(self, y)
    }
}

impl<X> core::ops::BitOr<An<X>> for Net
where
    X: AudioNode + core::marker::Send + Sync + 'static,
{
    type Output = Net;
    #[inline]
    fn bitor(self, y: An<X>) -> Self::Output {
        Net::stack(self, Net::wrap(Box::new(y)))
    }
}

impl<X> core::ops::BitOr<Net> for An<X>
where
    X: AudioNode + core::marker::Send + Sync + 'static,
{
    type Output = Net;
    #[inline]
    fn bitor(self, y: Net) -> Self::Output {
        Net::stack(Net::wrap(Box::new(self)), y)
    }
}

impl core::ops::BitXor<Net> for Net {
    type Output = Net;
    #[inline]
    fn bitxor(self, y: Net) -> Self::Output {
        Net::branch(self, y)
    }
}

impl<X> core::ops::BitXor<An<X>> for Net
where
    X: AudioNode + core::marker::Send + Sync + 'static,
{
    type Output = Net;
    #[inline]
    fn bitxor(self, y: An<X>) -> Self::Output {
        Net::branch(self, Net::wrap(Box::new(y)))
    }
}

impl<X> core::ops::BitXor<Net> for An<X>
where
    X: AudioNode + core::marker::Send + Sync + 'static,
{
    type Output = Net;
    #[inline]
    fn bitxor(self, y: Net) -> Self::Output {
        Net::branch(Net::wrap(Box::new(self)), y)
    }
}

impl core::ops::Add<Net> for Net {
    type Output = Net;
    #[inline]
    fn add(self, y: Net) -> Self::Output {
        Net::binary(self, y, FrameAdd::new())
    }
}

impl<X> core::ops::Add<An<X>> for Net
where
    X: AudioNode + core::marker::Send + Sync + 'static,
{
    type Output = Net;
    #[inline]
    fn add(self, y: An<X>) -> Self::Output {
        Net::binary(self, Net::wrap(Box::new(y)), FrameAdd::new())
    }
}

impl<X> core::ops::Add<Net> for An<X>
where
    X: AudioNode + core::marker::Send + Sync + 'static,
{
    type Output = Net;
    #[inline]
    fn add(self, y: Net) -> Self::Output {
        Net::binary(Net::wrap(Box::new(self)), y, FrameAdd::new())
    }
}

impl core::ops::Sub<Net> for Net {
    type Output = Net;
    #[inline]
    fn sub(self, y: Net) -> Self::Output {
        Net::binary(self, y, FrameSub::new())
    }
}

impl<X> core::ops::Sub<An<X>> for Net
where
    X: AudioNode + core::marker::Send + Sync + 'static,
{
    type Output = Net;
    #[inline]
    fn sub(self, y: An<X>) -> Self::Output {
        Net::binary(self, Net::wrap(Box::new(y)), FrameSub::new())
    }
}

impl<X> core::ops::Sub<Net> for An<X>
where
    X: AudioNode + core::marker::Send + Sync + 'static,
{
    type Output = Net;
    #[inline]
    fn sub(self, y: Net) -> Self::Output {
        Net::binary(Net::wrap(Box::new(self)), y, FrameSub::new())
    }
}

impl core::ops::Mul<Net> for Net {
    type Output = Net;
    #[inline]
    fn mul(self, y: Net) -> Self::Output {
        Net::binary(self, y, FrameMul::new())
    }
}

impl<X> core::ops::Mul<An<X>> for Net
where
    X: AudioNode + core::marker::Send + Sync + 'static,
{
    type Output = Net;
    #[inline]
    fn mul(self, y: An<X>) -> Self::Output {
        Net::binary(self, Net::wrap(Box::new(y)), FrameMul::new())
    }
}

impl<X> core::ops::Mul<Net> for An<X>
where
    X: AudioNode + core::marker::Send + Sync + 'static,
{
    type Output = Net;
    #[inline]
    fn mul(self, y: Net) -> Self::Output {
        Net::binary(Net::wrap(Box::new(self)), y, FrameMul::new())
    }
}

impl core::ops::Add<f32> for Net {
    type Output = Net;
    #[inline]
    fn add(self, y: f32) -> Self::Output {
        let n = self.outputs();
        self + Net::scalar(n, y)
    }
}

impl core::ops::Add<Net> for f32 {
    type Output = Net;
    #[inline]
    fn add(self, y: Net) -> Self::Output {
        let n = y.outputs();
        Net::scalar(n, self) + y
    }
}

impl core::ops::Sub<f32> for Net {
    type Output = Net;
    #[inline]
    fn sub(self, y: f32) -> Self::Output {
        let n = self.outputs();
        self - Net::scalar(n, y)
    }
}

impl core::ops::Sub<Net> for f32 {
    type Output = Net;
    #[inline]
    fn sub(self, y: Net) -> Self::Output {
        let n = y.outputs();
        Net::scalar(n, self) - y
    }
}

impl core::ops::Mul<f32> for Net {
    type Output = Net;
    #[inline]
    fn mul(self, y: f32) -> Self::Output {
        let n = self.outputs();
        self * Net::scalar(n, y)
    }
}

impl core::ops::Mul<Net> for f32 {
    type Output = Net;
    #[inline]
    fn mul(self, y: Net) -> Self::Output {
        let n = y.outputs();
        Net::scalar(n, self) * y
    }
}

#[cfg(test)]
mod pending_clone_proof {
    use super::*;

    /// The original bug shape must not compile.
    ///
    /// `clone_isolated` -> `reset()` is what silenced live playback. With
    /// `PendingClone` it is a type error, which is the whole point of the type.
    #[test]
    fn isolate_is_the_only_way_out() {
        let mut net = Net::new(0, 1);
        net.chain(Box::new(crate::prelude::dc(1.0)));
        let id = net.ids().next().copied().unwrap();
        let pending = net.clone_isolated(id).expect("target has outputs");

        // pending.reset();          // compile error: no such method
        // pending.tick(&[], &mut o); // compile error: not an AudioUnit
        let mut usable: Net = pending.isolate();
        usable.reset(); // only legal after isolating
        assert_eq!(usable.outputs(), 1);
    }
}
