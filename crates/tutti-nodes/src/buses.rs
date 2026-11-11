//! The buses a node declares, so the graph's dry path (a mix, a bypass) knows
//! where its input goes (doc 013, "As built (B3)"): the graph refuses one it
//! would have to guess.

use tutti_graph::{Bus, BusRole, Buses};
use tutti_types::{BusTopology, ChannelLayout, ChannelTopology};

/// A main bus of `width` channels at the engine's speakers for that width
/// (`ChannelTopology::assumed`), so the graph's mix and bypass fold a mono
/// input onto both sides of a stereo output. A width with no order names no
/// placement: the graph passes it only onto the same width.
pub(crate) fn main_bus(width: ChannelLayout) -> Buses {
    ChannelTopology::assumed(width)
        .and_then(|t| BusTopology::try_from(&t).ok())
        .map_or(Buses::of_width(width), Buses::main)
}

/// The inputs of a keyed processor: the audio on a main bus of `width`, then
/// its key on a [`Sidechain`](BusRole::Sidechain) bus as wide, which the dry
/// path never reads.
pub(crate) fn keyed(width: ChannelLayout) -> Buses {
    let main = main_bus(width);
    match BusTopology::discrete(width) {
        Some(key) if !main.is_empty() => main
            .with(Bus {
                topology: key,
                role: BusRole::Sidechain,
            })
            .unwrap_or(main),
        _ => main,
    }
}
