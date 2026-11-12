//! The buses a node declares, so the graph's dry path (a mix, a bypass) knows
//! where its input goes (doc 013, "As built (B3)"): the graph refuses one it
//! would have to guess.

use tutti_graph::Buses;
use tutti_types::{BusTopology, ChannelLayout};

/// A main bus of `width` channels at the engine's speakers for that width
/// (`ChannelTopology::assumed`), so the graph's mix and bypass fold a mono
/// input onto both sides of a stereo output. A width with no order names no
/// placement: the graph passes it only onto the same width. Past a bus's width
/// there is no bus; the node's ports say how many channels it has.
pub(crate) fn main_bus(width: ChannelLayout) -> Buses {
    BusTopology::assumed(width).map_or(Buses::NONE, Buses::main)
}
