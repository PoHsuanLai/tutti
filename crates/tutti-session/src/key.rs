//! Typed keys, ports and the one wiring vocabulary of the session's
//! [`Graph`] (doc 013, "The session graph's surface").
//!
//! - [`Key<N>`] is a node's identity with its type: [`Graph::add`] and
//!   [`Graph::insert`] return one, and every call that takes a node takes
//!   `impl Into<NodeKey>`, so a [`Key`] and an erased [`NodeKey`] both work.
//! - [`OutPort`] and [`InPort`] name one channel: a node's output or input
//!   port (`key.out(0)`, `key.input(1)`), a global input channel
//!   ([`Root::input`]) or a global output channel ([`Root::output`]).
//! - [`Graph::connect`] takes an [`Outlet`] and an [`Inlet`] of the same kind
//!   of wire: one channel, a node's whole main bus, or a bus edge.

use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;

use tutti_graph::{BusRef, BusSource, NodeKey};
use tutti_types::Amplitude;

use crate::{ConnectError, EdgeGainError, Graph};

mod sealed {
    pub trait Sealed {}
}
use sealed::Sealed;

/// A node's key, with the node's type: the handle [`Graph::add`] and
/// [`Graph::insert`] return.
///
/// It is a [`NodeKey`] and nothing more at run time (`Copy`, `Eq`, `Hash`),
/// so a key can be kept, compared and sent anywhere a node is taken
/// (`impl Into<NodeKey>`). The type is what makes a node's own params and
/// ports methods of its key rather than numbers.
pub struct Key<N: ?Sized> {
    node: NodeKey,
    kind: PhantomData<fn() -> N>,
}

impl<N: ?Sized> Key<N> {
    pub(crate) const fn new(node: NodeKey) -> Self {
        Self {
            node,
            kind: PhantomData,
        }
    }

    /// The key without its type.
    #[must_use]
    pub const fn erase(self) -> NodeKey {
        self.node
    }

    /// Output port `index` of the node.
    #[must_use]
    pub const fn out(self, index: usize) -> OutPort {
        OutPort(OutEnd::Node(self.node, index))
    }

    /// Input port `index` of the node.
    #[must_use]
    pub const fn input(self, index: usize) -> InPort {
        InPort(InEnd::Node(self.node, index))
    }

    /// Output bus `bus` of the node, as a source of a bus edge.
    #[must_use]
    pub const fn bus_out(self, bus: u8) -> BusSource {
        BusSource::Node(BusRef::new(self.node, bus))
    }

    /// Input bus `bus` of the node, as a sink of a bus edge.
    #[must_use]
    pub const fn bus_in(self, bus: u8) -> BusRef {
        BusRef::new(self.node, bus)
    }
}

impl<N: ?Sized> Clone for Key<N> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<N: ?Sized> Copy for Key<N> {}
impl<N: ?Sized> PartialEq for Key<N> {
    fn eq(&self, other: &Self) -> bool {
        self.node == other.node
    }
}
impl<N: ?Sized> Eq for Key<N> {}
impl<N: ?Sized> PartialOrd for Key<N> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl<N: ?Sized> Ord for Key<N> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.node.cmp(&other.node)
    }
}
impl<N: ?Sized> Hash for Key<N> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.node.hash(state);
    }
}
impl<N: ?Sized> fmt::Debug for Key<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Key<{}>({})", std::any::type_name::<N>(), self.node.0)
    }
}
impl<N: ?Sized> From<Key<N>> for NodeKey {
    fn from(key: Key<N>) -> Self {
        key.node
    }
}

/// One channel a signal leaves from: an output port of a node, or a channel
/// of the graph's own global input. Made by [`Key::out`], [`OutPort::new`]
/// and [`Root::input`].
///
/// The index is a port number of the node's declared ports, a count and not
/// a unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OutPort(OutEnd);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum OutEnd {
    Node(NodeKey, usize),
    Input(usize),
}

impl OutPort {
    /// Output port `index` of `node`.
    #[must_use]
    pub fn new(node: impl Into<NodeKey>, index: usize) -> Self {
        Self(OutEnd::Node(node.into(), index))
    }

    /// The node it leaves, `None` for a global input channel.
    #[must_use]
    pub const fn node(self) -> Option<NodeKey> {
        match self.0 {
            OutEnd::Node(node, _) => Some(node),
            OutEnd::Input(_) => None,
        }
    }

    /// The port's index on its node, or the global input channel.
    #[must_use]
    pub const fn index(self) -> usize {
        match self.0 {
            OutEnd::Node(_, index) | OutEnd::Input(index) => index,
        }
    }

    pub(crate) const fn end(self) -> OutEnd {
        self.0
    }
}

/// One channel a signal arrives at: an input port of a node, or a channel of
/// the graph's global output (the root's). Made by [`Key::input`],
/// [`InPort::new`] and [`Root::output`].
///
/// The index is a port number of the node's declared ports, a count and not
/// a unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InPort(InEnd);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum InEnd {
    Node(NodeKey, usize),
    Output(usize),
}

impl InPort {
    /// Input port `index` of `node`.
    #[must_use]
    pub fn new(node: impl Into<NodeKey>, index: usize) -> Self {
        Self(InEnd::Node(node.into(), index))
    }

    /// The node it arrives at, `None` for a global output channel.
    #[must_use]
    pub const fn node(self) -> Option<NodeKey> {
        match self.0 {
            InEnd::Node(node, _) => Some(node),
            InEnd::Output(_) => None,
        }
    }

    /// The port's index on its node, or the global output channel.
    #[must_use]
    pub const fn index(self) -> usize {
        match self.0 {
            InEnd::Node(_, index) | InEnd::Output(index) => index,
        }
    }

    pub(crate) const fn end(self) -> InEnd {
        self.0
    }
}

/// The graph's root: its global inputs and its global outputs.
///
/// A value of its own rather than a method of [`Graph`], so naming a root
/// channel borrows nothing: `s.graph().connect(osc.out(0), Root::output(0))`
/// is one borrow of the session. As an [`Inlet`] it is the sink of a node's
/// main bus ([`Graph::connect`] channel by channel onto the outputs) and of a
/// bus edge (folded to the topology the root was declared with).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Root;

impl Root {
    /// Global output channel `index`, as a sink.
    #[must_use]
    pub const fn output(index: usize) -> InPort {
        InPort(InEnd::Output(index))
    }

    /// Global input channel `index` (a hardware or host input), as a source.
    #[must_use]
    pub const fn input(index: usize) -> OutPort {
        OutPort(OutEnd::Input(index))
    }
}

/// One audio edge, named: what feeds which channel. What
/// [`Graph::connect`] adds and a modulation route onto its gain
/// ([`ModTarget`](crate::ModTarget), feature `modulation`) drives: the
/// **gain** of an edge is a weight of the sum its sink holds (doc 013, "Edge
/// gain"). The edge must exist; a target whose edge is not in the graph yet
/// is unresolved until it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Edge {
    from: OutPort,
    to: InPort,
}

impl Edge {
    /// The edge from `from` into `to`.
    #[must_use]
    pub const fn new(from: OutPort, to: InPort) -> Self {
        Self { from, to }
    }

    /// What feeds it.
    #[must_use]
    pub const fn source(self) -> OutPort {
        self.from
    }

    /// Where it goes.
    #[must_use]
    pub const fn sink(self) -> InPort {
        self.to
    }

    /// Whether the edge touches `node` (as its sink or its source).
    #[cfg(feature = "modulation")]
    pub(crate) fn touches(self, node: NodeKey) -> bool {
        self.from.node() == Some(node) || self.to.node() == Some(node)
    }

    /// The edge's gain in `graph`, if it has one: `Some` for a weighted
    /// edge, `None` for a plain one (a unity weight is an entry, so it is
    /// `Some(1.0)`; an edge with no entry is aliased, not weighted, and is not
    /// reported as unity) and for an edge that is not in the graph.
    ///
    /// Once the graph has committed the edge's weight this reads its live
    /// cell, which is also the knob of a modulation route onto the edge.
    #[must_use]
    pub fn gain(self, graph: &Graph) -> Option<Amplitude> {
        graph.edge_gain_of(self)
    }

    /// Sets the gain of the edge: a multiplier on what `from` adds to `to`'s
    /// sum. `1.0` is the plain sum (bit for bit, but for a `-0.0` sample into
    /// a one-edge sink), `0.0` mutes the edge, a negative gain inverts
    /// polarity. A move of the gain is ramped over one block.
    ///
    /// **This is the knob, routed or not.** A modulation route onto the edge
    /// (`Mods::route`, feature `modulation`) moves the gain around this value
    /// and removing the route returns to it: the one writer of the gain is the
    /// edge's cell.
    ///
    /// Giving a plain edge its first gain is structural (the next
    /// [`update`](crate::Session::update) commits a weighted sum, as adding a
    /// source does); changing the gain of a weighted edge is a control and
    /// marks nothing edited.
    ///
    /// # Errors
    ///
    /// [`EdgeGainError::NoSuchEdge`] when the edge is not in the graph,
    /// [`EdgeGainError::BusLane`] when it is a lane of a bus edge, and
    /// [`EdgeGainError::NotFinite`]; the graph is unchanged.
    pub fn set_gain(self, graph: &mut Graph, gain: Amplitude) -> Result<(), EdgeGainError> {
        graph.set_edge_gain_of(self, gain)
    }

    /// Makes the edge plain again: no gain, aliased when it is its sink's only
    /// source. Returns whether it had one. Structural, so it marks the graph
    /// edited when it did. A modulation route onto the edge makes the gain
    /// again (at unity) the next time the declarations apply: remove the route
    /// first.
    pub fn clear_gain(self, graph: &mut Graph) -> bool {
        graph.clear_edge_gain_of(self)
    }
}

/// The kinds of wire [`Graph::connect`] makes. Sealed: a source and a sink
/// connect when they are the same kind.
pub trait Wire: Sealed {
    #[doc(hidden)]
    type Out: Copy;
    #[doc(hidden)]
    type In: Copy;
    #[doc(hidden)]
    fn connect(
        graph: &mut Graph,
        from: Self::Out,
        to: Self::In,
    ) -> Result<Vec<(Edge, bool)>, ConnectError>;
    #[doc(hidden)]
    fn disconnect(graph: &mut Graph, from: Self::Out, to: Self::In) -> bool;
}

/// One channel to one channel: an [`OutPort`] into an [`InPort`], and an edge
/// with a gain.
#[derive(Clone, Copy, Debug)]
pub struct Channel;

/// A node's whole main output bus into another node's main input bus, or the
/// root's outputs, channel by channel: one edge per channel, each with a
/// gain.
#[derive(Clone, Copy, Debug)]
pub struct MainBus;

/// A bus edge: a fold between two declared bus topologies. Its lanes carry
/// the fold's weights and no gain of their own.
#[derive(Clone, Copy, Debug)]
pub struct Bus;

/// The wires whose edges carry a gain of their own.
pub trait Weighted: Wire {}
impl Weighted for Channel {}
impl Weighted for MainBus {}

/// Something a signal leaves from: [`OutPort`] (one channel), a [`Key`] or
/// [`NodeKey`] (the node's whole main bus) or a [`BusSource`]. Sealed.
pub trait Outlet: Sealed {
    /// The kind of wire it starts.
    type Wire: Wire;
    #[doc(hidden)]
    fn out(self) -> <Self::Wire as Wire>::Out;
}

/// Something a signal arrives at, for a wire of kind `W`: [`InPort`] (one
/// channel), a [`Key`] or [`NodeKey`] (the node's whole main bus), a
/// [`BusRef`], or the [`Root`]. Sealed.
pub trait Inlet<W: Wire>: Sealed {
    #[doc(hidden)]
    fn sink(self) -> W::In;
}

/// A main-bus sink: a node, or the root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MainSink {
    #[doc(hidden)]
    Node(NodeKey),
    #[doc(hidden)]
    Root,
}

/// A bus-edge sink: a node's input bus, or the root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BusSink {
    #[doc(hidden)]
    Node(BusRef),
    #[doc(hidden)]
    Root,
}

impl Sealed for OutPort {}
impl Sealed for InPort {}
impl Sealed for NodeKey {}
impl<N: ?Sized> Sealed for Key<N> {}
impl Sealed for BusSource {}
impl Sealed for BusRef {}
impl Sealed for Root {}
impl Sealed for Channel {}
impl Sealed for MainBus {}
impl Sealed for Bus {}

impl Wire for Channel {
    type Out = OutPort;
    type In = InPort;
    fn connect(
        graph: &mut Graph,
        from: OutPort,
        to: InPort,
    ) -> Result<Vec<(Edge, bool)>, ConnectError> {
        let added = graph.add_edge(from, to)?;
        Ok(vec![(Edge::new(from, to), added)])
    }
    fn disconnect(graph: &mut Graph, from: OutPort, to: InPort) -> bool {
        graph.remove_edge(from, to)
    }
}

impl Wire for MainBus {
    type Out = NodeKey;
    type In = MainSink;
    fn connect(
        graph: &mut Graph,
        from: NodeKey,
        to: MainSink,
    ) -> Result<Vec<(Edge, bool)>, ConnectError> {
        let wires = graph.main_wires(from, to)?;
        let mut edges = Vec::with_capacity(wires.len());
        for (out, input) in wires {
            let added = graph.add_edge(out, input)?;
            edges.push((Edge::new(out, input), added));
        }
        Ok(edges)
    }
    fn disconnect(graph: &mut Graph, from: NodeKey, to: MainSink) -> bool {
        let Ok(wires) = graph.main_wires(from, to) else {
            return false;
        };
        let mut removed = false;
        for (out, input) in wires {
            removed |= graph.remove_edge(out, input);
        }
        removed
    }
}

impl Wire for Bus {
    type Out = BusSource;
    type In = BusSink;
    fn connect(
        graph: &mut Graph,
        from: BusSource,
        to: BusSink,
    ) -> Result<Vec<(Edge, bool)>, ConnectError> {
        graph.add_bus_edge(from, to, false)?;
        Ok(Vec::new())
    }
    fn disconnect(graph: &mut Graph, from: BusSource, to: BusSink) -> bool {
        graph.remove_bus_edge(from, to)
    }
}

impl Outlet for OutPort {
    type Wire = Channel;
    fn out(self) -> OutPort {
        self
    }
}
impl Outlet for NodeKey {
    type Wire = MainBus;
    fn out(self) -> NodeKey {
        self
    }
}
impl<N: ?Sized> Outlet for Key<N> {
    type Wire = MainBus;
    fn out(self) -> NodeKey {
        self.node
    }
}
impl Outlet for BusSource {
    type Wire = Bus;
    fn out(self) -> BusSource {
        self
    }
}

impl Inlet<Channel> for InPort {
    fn sink(self) -> InPort {
        self
    }
}
impl Inlet<MainBus> for NodeKey {
    fn sink(self) -> MainSink {
        MainSink::Node(self)
    }
}
impl<N: ?Sized> Inlet<MainBus> for Key<N> {
    fn sink(self) -> MainSink {
        MainSink::Node(self.node)
    }
}
impl Inlet<MainBus> for Root {
    fn sink(self) -> MainSink {
        MainSink::Root
    }
}
impl Inlet<Bus> for BusRef {
    fn sink(self) -> BusSink {
        BusSink::Node(self)
    }
}
impl Inlet<Bus> for Root {
    fn sink(self) -> BusSink {
        BusSink::Root
    }
}

/// The edges a [`Graph::connect`] just made, borrowing the graph so a gain
/// can be set on them: `g.connect(a.out(0), b.input(0))?.gain(half)?`.
///
/// Dropping it keeps the edges as they are. Get the [`Edge`]s with
/// [`edges`](Self::edges) to keep a handle (a modulation target, a later
/// gain).
#[derive(Debug)]
pub struct Connection<'g, W: Wire> {
    graph: &'g mut Graph,
    edges: Vec<(Edge, bool)>,
    wire: PhantomData<fn() -> W>,
}

impl<'g, W: Wire> Connection<'g, W> {
    pub(crate) fn new(graph: &'g mut Graph, edges: Vec<(Edge, bool)>) -> Self {
        Self {
            graph,
            edges,
            wire: PhantomData,
        }
    }

    /// The edges the call connected: one for a [`Channel`], one per channel
    /// for a [`MainBus`], none for a [`Bus`] (its lanes are the fold's). An
    /// edge the graph already held is listed too.
    #[must_use]
    pub fn edges(&self) -> Vec<Edge> {
        self.edges.iter().map(|(edge, _)| *edge).collect()
    }
}

impl<W: Weighted> Connection<'_, W> {
    /// Weights every edge of the connection by `gain` (the edge gain of doc
    /// 013, "Edge gain"): the edges enter the sum at `gain` rather than
    /// unity. An edge the graph already held only has its gain set.
    ///
    /// # Errors
    ///
    /// [`EdgeGainError::NotFinite`]; the edges this call added are taken
    /// out again, so nothing is added.
    pub fn gain(self, gain: Amplitude) -> Result<Self, EdgeGainError> {
        if !gain.get().is_finite() {
            for (edge, added) in &self.edges {
                if *added {
                    self.graph.remove_edge(edge.from, edge.to);
                }
            }
            return Err(EdgeGainError::NotFinite);
        }
        for (edge, _) in &self.edges {
            edge.set_gain(self.graph, gain)?;
        }
        Ok(self)
    }
}
