//! The session's name for one edge of the graph, and the refusal of a gain
//! written to it (doc 013, "Edge gain").

use thiserror::Error;
use tutti_graph::{Edge, EdgeId, NodeKey, Sink};

use crate::graph::{Graph, GraphSource};

/// Where an edge goes: input `port` of `node`, or global output `channel`.
/// The session's key form of [`Sink`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EdgeSink {
    /// Audio input `port` of `node`.
    Port {
        /// The node.
        node: NodeKey,
        /// Its input port.
        port: usize,
    },
    /// Global output `channel`.
    Output(usize),
}

/// One edge of the session's graph, named as the session's fan-in methods
/// name it ([`Graph::add_source`](crate::Graph::add_source),
/// [`Graph::add_output_source`](crate::Graph::add_output_source)): what feeds
/// which sink. The session's key form of `tutti_graph::EdgeId`, as
/// `ParamRef` is of a param port. A modulation route onto it
/// (`ModTarget`, feature `modulation`) drives the **gain** of that edge, a weight of the sum
/// its sink holds (doc 013, "Edge gain"); the edge must exist, and a target
/// whose edge is not in the graph yet is unresolved until it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EdgeRef {
    /// Where the edge goes.
    pub sink: EdgeSink,
    /// What feeds it.
    pub from: GraphSource,
}

impl EdgeRef {
    /// The edge from `from` into input `port` of `node`.
    #[must_use]
    pub const fn port(node: NodeKey, port: usize, from: GraphSource) -> Self {
        Self {
            sink: EdgeSink::Port { node, port },
            from,
        }
    }

    /// The edge from `from` into global output `channel`.
    #[must_use]
    pub const fn output(channel: usize, from: GraphSource) -> Self {
        Self {
            sink: EdgeSink::Output(channel),
            from,
        }
    }

    /// The graph's name for the edge (a direct one: a feedback edge has no
    /// session name yet).
    pub(crate) fn id(self) -> EdgeId {
        EdgeId {
            sink: match self.sink {
                EdgeSink::Port { node, port } => Sink::Port(tutti_graph::InPort {
                    node,
                    port: port as u16,
                }),
                EdgeSink::Output(channel) => Sink::Output(channel as u16),
            },
            from: Edge::Direct(Graph::lower(self.from)),
        }
    }

    /// Whether the edge touches `node` (as its sink or its source).
    #[cfg(feature = "modulation")]
    pub(crate) fn touches(self, node: NodeKey) -> bool {
        matches!(self.sink, EdgeSink::Port { node: n, .. } if n == node)
            || matches!(self.from, GraphSource::Node(n, _) if n == node)
    }
}

/// Why the session refused to give an edge a gain
/// ([`Graph::set_edge_gain`](crate::Graph::set_edge_gain),
/// [`Graph::add_source_weighted`](crate::Graph::add_source_weighted)). A
/// refusal changes nothing.
#[derive(Clone, Copy, Debug, PartialEq, Error)]
#[non_exhaustive]
pub enum EdgeGainError {
    /// The edge is not in the graph: nothing feeds the sink from that source
    /// (and [`GraphSource::Silence`] is never an edge).
    #[error("no such edge: {0:?}")]
    NoSuchEdge(EdgeRef),
    /// The gain is not finite (NaN or infinite): a weight must be a number
    /// the sum can multiply by.
    #[error("an edge gain must be finite")]
    NotFinite,
    /// The edge is a lane a bus edge lowers to
    /// ([`Graph::connect_bus`](crate::Graph::connect_bus)): its weight is the
    /// fold's, and a bus edge carries no gain of its own.
    #[error("the edge is a lane of a bus edge: {0:?}")]
    BusLane(EdgeRef),
}
