//! The buses a node declares, so the graph's dry path (a mix, a bypass) knows
//! where its input goes (doc 013, "As built (B3)"): the graph refuses one it
//! would have to guess.

use tutti_graph::{Bus, BusRole, Buses, InBus, Shape};
use tutti_types::{BusTopology, ChannelLayout};

/// A main bus of `width` channels at the engine's speakers for that width
/// (`ChannelTopology::assumed`), so the graph's mix and bypass fold a mono
/// input onto both sides of a stereo output. A width with no order names no
/// placement: the graph passes it only onto the same width. Past a bus's width
/// there is no bus; the node's ports say how many channels it has.
pub(crate) fn main_bus(width: ChannelLayout) -> Buses {
    BusTopology::assumed(width).map_or(Buses::NONE, Buses::main)
}

/// A keyed processor's ports, and its key's handle: its audio on a main bus of
/// `width` channels, its key on a [`Sidechain`](BusRole::Sidechain) bus as
/// wide, its output the main bus again. Built once on the control side (a
/// topology allocates). The node reads its audio as the main bus
/// (`Io::input`, `Inputs::get`) and its key through the handle, so what the
/// graph's dry path passes and what the node reads are one statement.
///
/// # Panics
///
/// Past [`MAX_BUS_CHANNELS`](tutti_types::MAX_BUS_CHANNELS): a key that wide
/// fits no bus.
pub(crate) fn keyed(width: ChannelLayout) -> (Shape, InBus) {
    let main = main_bus(width);
    let key = BusTopology::unassigned(width).expect("a keyed node is at most 16 channels wide");
    let inputs = main
        .with(Bus::new(key, BusRole::Sidechain))
        .expect("a main bus and a key are two buses");
    let shape = Shape::buses(inputs, main);
    let handle = shape.input_bus(1).expect("the key is bus 1");
    (shape, handle)
}
