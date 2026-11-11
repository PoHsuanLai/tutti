//! The refusal of a gain written to an edge (doc 013, "Edge gain"). The edge
//! itself, [`Edge`], is in `key.rs` beside the ports it joins.

use thiserror::Error;

use crate::Edge;

/// Why the session refused to give an edge a gain
/// ([`Edge::set_gain`](crate::Edge::set_gain),
/// [`Connection::gain`](crate::Connection::gain)). A refusal changes nothing.
#[derive(Clone, Copy, Debug, PartialEq, Error)]
#[non_exhaustive]
pub enum EdgeGainError {
    /// The edge is not in the graph: nothing feeds the sink from that source.
    #[error("no such edge: {0:?}")]
    NoSuchEdge(Edge),
    /// The gain is not finite (NaN or infinite): a weight must be a number
    /// the sum can multiply by.
    #[error("an edge gain must be finite")]
    NotFinite,
    /// The edge is a lane a bus edge lowers to
    /// ([`Graph::connect`](crate::Graph::connect) between buses): its weight
    /// is the fold's, and a bus edge carries no gain of its own.
    #[error("the edge is a lane of a bus edge: {0:?}")]
    BusLane(Edge),
}
