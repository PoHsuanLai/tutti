//! The audio graph as a **value**: build it, fold over it, compare it, hash it.
//!
//! [`Topology`] **is** the graph: nodes by stable [`NodeKey`], edges keyed by
//! the sink port they feed, and the global outputs. `tutti-graph`'s compiler
//! interprets it (a `Topology` in, an immutable plan out), and nothing reads a
//! topology back out of a plan. Because it is a plain value, `Eq` compares two
//! graphs, `Hash` keys a cache on one, and every static property is a pure
//! fold over it: evaluation order ([`Topology::topo_order`]), width agreement
//! ([`Topology::validate`]), latency ([`latency::plan`](crate::latency::plan))
//! and tail ([`graph_tail`](crate::graph_tail)).
//!
//! # Validation instead of a type index
//!
//! Channel widths are runtime data (a file's channel count, a plugin's bus
//! arity, a device's layout), so a stereo-into-mono edge cannot be a compile
//! error. [`Topology::validate`] is the replacement: one total function that
//! returns every disagreement at once, run on the control thread, whose
//! *result* is carried in the type as [`Valid`].
//!
//! # Examples
//!
//! ```
//! use tutti_types::graph::{Edge, InPort, NodeSpec, OutPort, Source};
//! use tutti_types::{ChannelLayout, NodeKey, Topology};
//!
//! let (osc, gain) = (NodeKey(1), NodeKey(2));
//! let mut g = Topology::default();
//! g.nodes.insert(osc, NodeSpec::new("osc", ChannelLayout::EMPTY, ChannelLayout::MONO));
//! g.nodes.insert(gain, NodeSpec::new("gain", ChannelLayout::MONO, ChannelLayout::MONO));
//! g.edges.insert(
//!     InPort { node: gain, port: 0 },
//!     Edge::Direct(Source::Node(OutPort { node: osc, port: 0 })),
//! );
//! g.outputs = vec![Source::Node(OutPort { node: gain, port: 0 })];
//!
//! let valid = g.validate().expect("well formed");
//! assert_eq!(valid.get().topo_order(), Ok(vec![osc, gain]));
//! ```

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::latency::{Feed, LatencyGraph};
use crate::tail::TailGraph;
use crate::value::{Samples, Tail};
use crate::ChannelLayout;

/// A stable identity for a node, chosen by the **author** of the topology.
///
/// A `NodeKey` is supplied from outside (a Bevy `Entity`'s bits, a document
/// node id, a test's literal), so it stays the same across any number of
/// recompiles and across a crossfade that replaces the node: a handle a caller
/// stored never goes stale. The runtime's own id is derived from it while
/// compiling and never escapes.
///
/// A host with no id of its own to supply takes one from
/// [`NodeKey::fresh`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeKey(pub u64);

impl NodeKey {
    /// Returns a key no earlier call in this process returned: a
    /// process-wide counter from 0, one step per call, from any thread.
    ///
    /// Unique among its own calls only. A key a host writes itself (an
    /// `Entity`'s bits, a literal) is not reserved against it, so a host
    /// mixing the two keeps them in ranges that cannot meet.
    pub fn fresh() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        Self(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }
}

/// Which **output** port of which node a signal leaves from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OutPort {
    /// The node the signal leaves.
    pub node: NodeKey,
    /// Which of its output ports.
    pub port: u16,
}

/// Which **input** port of which node a signal arrives at.
///
/// The key of [`Topology::edges`] — see that field for why.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InPort {
    /// The node the signal arrives at.
    pub node: NodeKey,
    /// Which of its input ports.
    pub port: u16,
}

/// What feeds one sink port.
///
/// Fan-in is unrepresentable: an [`InPort`] is a map *key*, so it has at most
/// one source. Summing is a node, never a property of an edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Source {
    /// An output port of another node in this topology.
    Node(OutPort),
    /// A channel of the topology's own global input.
    Global(u16),
    /// Explicit silence. Distinct from *absent*: an author who wired a port to
    /// nothing on purpose has said something a missing key has not.
    Zero,
}

/// A source read through an explicit delay, closing a cycle.
///
/// # The delay is part of the edge
///
/// A feedback edge delays by exactly [`delay`](Self::delay) frames, whatever
/// the block size. It is *not* "one block", because the block size is a
/// property of the runtime, not of the graph: a bounce prepared at a larger
/// maximum block than live playback would otherwise loop at a different
/// period, and a bounce must sound like what was heard.
///
/// A block cannot read samples it has not produced yet, so an interpreter
/// needs `delay >= its maximum block`, and `tutti-graph` refuses to compile a
/// shorter one. **An offline render must therefore be prepared with a maximum
/// block no larger than the smallest feedback delay in the graph**; the
/// compile error enforces it rather than silently changing the sound.
/// [`one_block`](Self::one_block) spells the common case: a delay of one
/// block at the size the author chose.
///
/// The rest of this module's "last block" wording means this delay.
///
/// **The only way to express feedback.** A [`Source`] edge that closes a cycle
/// is an [`Invalid::Cycle`]; a feedback edge is a cycle the author declared. The
/// mandatory delay is a property of the *edge kind*, not a node the author has
/// to remember to insert, so "I forgot the delay" is unrepresentable rather than
/// a hang.
///
/// Named `FeedbackFrom` rather than `Feedback` because
/// [`crate::value::Feedback`] is already the recirculation-coefficient unit; two
/// types called `Feedback` in one crate is a collision, not a pun.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FeedbackFrom {
    /// The port whose delayed output feeds the sink.
    pub from: OutPort,
    /// How far back, in frames. See the type docs for the lower bound.
    pub delay: Samples,
}

impl FeedbackFrom {
    /// Creates a feedback edge from `from`, delayed by `delay` frames.
    pub const fn new(from: OutPort, delay: Samples) -> Self {
        Self { from, delay }
    }

    /// Creates a feedback edge from `from`, delayed by one block of `block` frames —
    /// the block size the author designed the loop for, fixed here so every
    /// interpreter (live or offline) renders the same loop.
    pub const fn one_block(from: OutPort, block: Samples) -> Self {
        Self { from, delay: block }
    }
}

/// One incoming connection: a normal edge, or a feedback edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Edge {
    /// This block's value, from the named source.
    Direct(Source),
    /// Last block's value, from the named port — a declared cycle.
    Feedback(FeedbackFrom),
}

/// A parameter's initial value, by name.
///
/// Block-rate by construction: a [`Topology`] is compiled on the control thread,
/// so a value here is read when a node is *built*, never per sample. A genuinely
/// per-sample control is a signal and belongs on an input port.
#[derive(Clone, Copy, Debug)]
pub enum ParamValue {
    /// A continuous value, in the parameter's own unit.
    Scalar(f32),
    /// A switch.
    Bool(bool),
    /// A choice from a discrete list.
    Index(u32),
}

// `PartialEq` and `Hash` are both hand-written over the same bit-pattern key, so
// that they agree — the `Hash`/`Eq` contract, and not a formality here: derived
// float equality says `0.0 == -0.0` while their bit patterns differ, so a
// derived `PartialEq` beside a bit-pattern `Hash` would put two "equal" specs in
// two hash buckets.
//
// Bit patterns are the right relation for both. This is a *structural*
// comparison deciding whether two graphs are the same graph, not a numeric one:
// two params differing by an ULP are two different graphs, and collapsing them
// would let a recompile keep a node built from the old number.
//
// It also makes `Eq` honest, which derived float equality cannot be — `NaN != NaN`
// is not reflexive, and a `NaN` param would otherwise make a spec unequal to
// itself and a `Topology` unfindable in its own map.
impl PartialEq for ParamValue {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl Eq for ParamValue {}

impl std::hash::Hash for ParamValue {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.key().hash(state);
    }
}

impl ParamValue {
    fn key(&self) -> (u8, u64) {
        match *self {
            ParamValue::Scalar(v) => (0, v.to_bits() as u64),
            ParamValue::Bool(b) => (1, b as u64),
            ParamValue::Index(i) => (2, i as u64),
        }
    }
}

/// What kind of node this is, by **catalog id** — never a boxed unit.
///
/// The whole point of the split: a spec is `Clone + Eq + Hash` and carries no
/// DSP state, so a test can compare two graphs and a recompile can diff them.
/// The unit itself (a `tutti_graph::Node`) lives in the runtime, under the
/// same `NodeKey`.
///
/// Deliberately **not** `Ord`: [`Tail`] has no ordering upstream (`Unknown` and
/// `Unbounded` are not points on a line), and inventing one here would be a
/// second, disagreeing definition of a value this crate already owns.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NodeSpec {
    /// Catalog id — `"gain"`, `"plugin:vst3:…"`, `"ext:my.reverb"`. Open on
    /// purpose: a closed enum here would mean a plugin format cannot add a kind,
    /// and the set of kinds is a host's to extend, not this crate's to enumerate.
    pub kind: String,
    /// Declared input width. [`Topology::validate`] checks incoming edges
    /// against it.
    pub inputs: ChannelLayout,
    /// Declared output width.
    pub outputs: ChannelLayout,
    /// **Involuntary** latency the node reports: frames it buffers as a side
    /// effect, a lookahead limiter's, never a delay line's. A voluntary delay is
    /// the effect, and compensating for it would drag every parallel path late.
    pub latency: Samples,
    /// How long it rings after its input stops.
    pub tail: Tail,
    /// Initial parameter values by name. `BTreeMap` so the whole spec hashes
    /// deterministically — a `HashMap` here would make `Topology: Hash` a lie.
    pub params: BTreeMap<String, ParamValue>,
}

impl NodeSpec {
    /// Creates a spec of `kind` at the given widths: no latency, no tail, no
    /// params.
    ///
    /// The common shape; amend with the builder that names the field you mean.
    /// A constructor rather than a struct literal so adding a field later does
    /// not break every call site that never cared about it.
    pub fn new(kind: impl Into<String>, inputs: ChannelLayout, outputs: ChannelLayout) -> Self {
        Self {
            kind: kind.into(),
            inputs,
            outputs,
            latency: Samples::ZERO,
            tail: Tail::None,
            params: BTreeMap::new(),
        }
    }

    /// Returns this spec with `latency` frames of involuntary latency.
    #[must_use]
    pub fn with_latency(mut self, latency: Samples) -> Self {
        self.latency = latency;
        self
    }

    /// Returns this spec with the given ring-out.
    #[must_use]
    pub fn with_tail(mut self, tail: Tail) -> Self {
        self.tail = tail;
        self
    }

    /// Returns this spec with one more initial parameter value.
    #[must_use]
    pub fn with_param(mut self, name: impl Into<String>, value: ParamValue) -> Self {
        self.params.insert(name.into(), value);
        self
    }

    /// Returns the scalar value of `name`, if it has one.
    ///
    /// The lookup a catalog performs to build a node, written once here rather
    /// than open-coded per kind.
    pub fn scalar(&self, name: &str) -> Option<f32> {
        match self.params.get(name) {
            Some(ParamValue::Scalar(v)) => Some(*v),
            _ => None,
        }
    }
}

/// The audio graph, as a value.
///
/// See the [module docs](self) for what it is for and an example. `Default`
/// is the empty graph with no global inputs.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Topology {
    /// Nodes by stable key. `BTreeMap` so iteration order — and therefore the
    /// hash, and therefore every derived answer — is deterministic.
    pub nodes: BTreeMap<NodeKey, NodeSpec>,
    /// **Keyed on the sink port.** This is what makes fan-in unrepresentable.
    pub edges: BTreeMap<InPort, Edge>,
    /// What feeds each global output channel, in channel order.
    pub outputs: Vec<Source>,
    /// The topology's own global input width — [`ChannelLayout::EMPTY`] for a
    /// master graph, which has none.
    pub inputs: ChannelLayout,
}

/// An empty graph with **no global inputs** — a master graph before anything is
/// added to it.
///
/// Hand-written: `ChannelLayout` has no `Default`, so the width is chosen here.
/// With no global inputs, a stray `Source::Global` edge is rejected by
/// [`Topology::validate`] rather than resolving against inputs nobody asked
/// for.
impl Default for Topology {
    fn default() -> Self {
        Self {
            nodes: BTreeMap::new(),
            edges: BTreeMap::new(),
            outputs: Vec::new(),
            inputs: ChannelLayout::EMPTY,
        }
    }
}

/// Every way a [`Topology`] can be malformed.
///
/// Returned as a list, never short-circuited: an author fixing a graph wants all
/// of them, and reporting one fault at a time turns a single edit into a
/// bisection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Invalid {
    /// An edge names a node that is not in [`Topology::nodes`].
    UnknownNode {
        /// The sink port whose edge names it.
        at: InPort,
        /// The key that is not in the map.
        missing: NodeKey,
    },
    /// An edge arrives at a port past the sink's declared input width.
    SinkPortOutOfRange {
        /// The out-of-range sink port.
        at: InPort,
        /// The sink's declared width.
        width: ChannelLayout,
    },
    /// An edge leaves from a port past the source's declared output width.
    SourcePortOutOfRange {
        /// The sink port whose edge is bad.
        at: InPort,
        /// The out-of-range source port.
        from: OutPort,
        /// The source's declared width.
        width: ChannelLayout,
    },
    /// A global-input edge names a channel past [`Topology::inputs`].
    GlobalInputOutOfRange {
        /// The sink port whose edge is bad.
        at: InPort,
        /// The named global channel.
        channel: u16,
        /// The topology's declared input width.
        width: ChannelLayout,
    },
    /// A global output channel is sourced from a node port that does not exist.
    OutputOutOfRange {
        /// Which output channel.
        channel: usize,
        /// The port it names.
        from: OutPort,
    },
    /// A cycle **not** broken by an [`Edge::Feedback`] edge.
    Cycle {
        /// The nodes the topological walk could not place.
        involving: Vec<NodeKey>,
    },
    /// A declared input port left unconnected.
    ///
    /// Reported, but **not fatal** — see [`Topology::validate`]. An unconnected
    /// input reads silence, which is well defined and is what a graph looks like
    /// mid-edit.
    Unconnected {
        /// The port nothing feeds.
        at: InPort,
    },
}

impl Invalid {
    /// Returns whether this fault blocks validity.
    ///
    /// Only [`Unconnected`](Self::Unconnected) does not.
    pub fn is_fatal(&self) -> bool {
        !matches!(self, Invalid::Unconnected { .. })
    }
}

/// A [`Topology`] that has passed [`validate`](Topology::validate).
///
/// Compiling takes one of these, not a bare [`Topology`], so "did anyone check
/// this?" is answered by the type rather than by convention. This is the nearest
/// available analogue of a type index: the check is still runtime, but its
/// *result* is carried in the type system from there on.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Valid(Topology);

impl Valid {
    /// Returns the checked topology.
    pub fn get(&self) -> &Topology {
        &self.0
    }

    /// Takes the checked topology back out.
    ///
    /// Consuming rather than clone-and-get, so a caller that means to *edit* the
    /// graph gives up the proof it was checked — which is the wrapper's point.
    pub fn into_inner(self) -> Topology {
        self.0
    }
}

impl Topology {
    /// Checks every structural property: that every edge names existing nodes
    /// and in-range ports, that global inputs and outputs resolve, and that
    /// every cycle is broken by a feedback edge.
    ///
    /// Clones the topology into the returned [`Valid`]; runs on the control
    /// thread.
    ///
    /// # Errors
    ///
    /// Returns **all** problems, not the first, when any is fatal.
    /// [`Unconnected`](Invalid::Unconnected) is reported but does not block
    /// validity: a partially built graph is renderable and reads silence there.
    /// A caller that wants to warn about half-wired ports asks
    /// [`unconnected`](Self::unconnected) directly, rather than inferring it
    /// from an error list that an `Ok` will not carry.
    pub fn validate(&self) -> Result<Valid, Vec<Invalid>> {
        let mut errs = Vec::new();

        for (&at, edge) in &self.edges {
            let Some(sink) = self.nodes.get(&at.node) else {
                errs.push(Invalid::UnknownNode {
                    at,
                    missing: at.node,
                });
                continue;
            };
            if at.port >= sink.inputs.count() {
                errs.push(Invalid::SinkPortOutOfRange {
                    at,
                    width: sink.inputs,
                });
            }
            let from = match *edge {
                Edge::Direct(Source::Node(p)) | Edge::Feedback(FeedbackFrom { from: p, .. }) => {
                    Some(p)
                }
                Edge::Direct(Source::Global(ch)) => {
                    if ch >= self.inputs.count() {
                        errs.push(Invalid::GlobalInputOutOfRange {
                            at,
                            channel: ch,
                            width: self.inputs,
                        });
                    }
                    None
                }
                Edge::Direct(Source::Zero) => None,
            };
            if let Some(from) = from {
                match self.nodes.get(&from.node) {
                    None => errs.push(Invalid::UnknownNode {
                        at,
                        missing: from.node,
                    }),
                    Some(src) if from.port >= src.outputs.count() => {
                        errs.push(Invalid::SourcePortOutOfRange {
                            at,
                            from,
                            width: src.outputs,
                        });
                    }
                    Some(_) => {}
                }
            }
        }

        for (channel, src) in self.outputs.iter().enumerate() {
            if let Source::Node(from) = *src {
                let resolves = self
                    .nodes
                    .get(&from.node)
                    .is_some_and(|n| from.port < n.outputs.count());
                if !resolves {
                    errs.push(Invalid::OutputOutOfRange { channel, from });
                }
            }
        }

        errs.extend(self.unconnected().map(|at| Invalid::Unconnected { at }));

        // Cycles over DIRECT edges only: a feedback edge is the author saying
        // "this one reads last block", so it is cut before the walk.
        if let Err(involving) = self.acyclic_order() {
            errs.push(Invalid::Cycle { involving });
        }

        if errs.iter().any(Invalid::is_fatal) {
            Err(errs)
        } else {
            Ok(Valid(self.clone()))
        }
    }

    /// Returns every declared input port nothing feeds, in key order.
    ///
    /// Not a fault (see [`validate`](Self::validate)) — exposed so a caller that
    /// wants to warn about a half-wired graph has an answer that does not depend
    /// on validation having failed.
    pub fn unconnected(&self) -> impl Iterator<Item = InPort> + '_ {
        self.nodes.iter().flat_map(move |(&node, spec)| {
            (0..spec.inputs.count())
                .map(move |port| InPort { node, port })
                .filter(move |at| !self.edges.contains_key(at))
        })
    }

    /// Direct-edge predecessors of `node`. Feedback edges are excluded — they
    /// carry last block's value, so they are not predecessors *this* block.
    fn direct_preds(&self, node: NodeKey) -> impl Iterator<Item = NodeKey> + '_ {
        self.edges
            .iter()
            .filter(move |(at, _)| at.node == node)
            .filter_map(|(_, e)| match e {
                Edge::Direct(Source::Node(p)) => Some(p.node),
                _ => None,
            })
    }

    /// Kahn's algorithm over the direct edges. `Err` carries the nodes left in
    /// the cycle.
    fn acyclic_order(&self) -> Result<Vec<NodeKey>, Vec<NodeKey>> {
        let mut in_degree: BTreeMap<NodeKey, usize> = self.nodes.keys().map(|&k| (k, 0)).collect();
        let mut dependents: HashMap<NodeKey, Vec<NodeKey>> = HashMap::new();

        for &node in self.nodes.keys() {
            for pred in self.direct_preds(node) {
                if self.nodes.contains_key(&pred) {
                    dependents.entry(pred).or_default().push(node);
                    *in_degree.get_mut(&node).expect("keyed from nodes") += 1;
                }
            }
        }

        // Seeding from a `BTreeMap` keeps the queue deterministic, which is what
        // makes `topo_order` a *function* of the value rather than merely one
        // valid answer — so a test can assert the exact vector.
        let mut queue: Vec<NodeKey> = in_degree
            .iter()
            .filter(|(_, &d)| d == 0)
            .map(|(&k, _)| k)
            .rev()
            .collect();

        let mut order = Vec::with_capacity(self.nodes.len());
        while let Some(n) = queue.pop() {
            order.push(n);
            let mut ready = Vec::new();
            for &dep in dependents.get(&n).into_iter().flatten() {
                let d = in_degree.get_mut(&dep).expect("keyed from nodes");
                *d -= 1;
                if *d == 0 {
                    ready.push(dep);
                }
            }
            ready.sort_unstable();
            queue.extend(ready.into_iter().rev());
        }

        if order.len() == self.nodes.len() {
            Ok(order)
        } else {
            let placed: HashSet<NodeKey> = order.iter().copied().collect();
            Err(self
                .nodes
                .keys()
                .copied()
                .filter(|k| !placed.contains(k))
                .collect())
        }
    }

    /// Returns the evaluation order: deterministic, and a pure function of the
    /// value.
    ///
    /// Feedback edges are cut before the walk, so they never order two nodes.
    ///
    /// # Errors
    ///
    /// The nodes a cycle of direct edges left unplaceable.
    pub fn topo_order(&self) -> Result<Vec<NodeKey>, Vec<NodeKey>> {
        self.acyclic_order()
    }
}

impl LatencyGraph for Topology {
    type Node = NodeKey;

    fn nodes(&self) -> impl Iterator<Item = NodeKey> {
        self.nodes.keys().copied()
    }

    fn latency(&self, node: NodeKey) -> Samples {
        self.nodes.get(&node).map_or(Samples::ZERO, |n| n.latency)
    }

    /// In **port order**: a node's output is [`Feed::Node`], a global input
    /// [`Feed::Outside`] (aligned at a merge point, as the compiler aligns
    /// it), and an unconnected or silent port [`Feed::None`].
    ///
    /// A feedback edge yields `Feed::None`: it carries last block's value, so
    /// it contributes no latency along this block's path, and reporting it as
    /// a live predecessor would put the walk back in the cycle the edge kind
    /// exists to cut.
    fn inputs(&self, node: NodeKey) -> impl Iterator<Item = Feed<NodeKey>> {
        let width = self.nodes.get(&node).map_or(0, |n| n.inputs.count());
        (0..width).map(move |port| match self.edges.get(&InPort { node, port }) {
            Some(Edge::Direct(Source::Node(p))) => Feed::Node(p.node),
            Some(Edge::Direct(Source::Global(_))) => Feed::Outside,
            _ => Feed::None,
        })
    }

    fn outputs(&self) -> impl Iterator<Item = Option<NodeKey>> {
        self.outputs.iter().map(|s| match s {
            Source::Node(p) => Some(p.node),
            _ => None,
        })
    }
}

impl TailGraph for Topology {
    fn tail(&self, node: NodeKey) -> Tail {
        self.nodes.get(&node).map_or(Tail::Unknown, |n| n.tail)
    }
}
