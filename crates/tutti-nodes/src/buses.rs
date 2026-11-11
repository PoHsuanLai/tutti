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

/// A keyed processor's ports: its audio on a main bus of `width` channels, then
/// its key on a [`Sidechain`](BusRole::Sidechain) bus as wide, and its output
/// the main bus again. Built once on the control side (a topology allocates):
/// the node declares [`inputs`](Self::inputs) and [`outputs`](Self::outputs),
/// and its `process` reads [`audio`](Self::audio) and [`key`](Self::key), the
/// ports those same buses cover, so what the graph's dry path passes and what
/// the node reads are one statement. Past a bus's width no bus is declared (the
/// graph then refuses a dry path) and the key follows the audio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Keyed {
    width: ChannelLayout,
    inputs: Buses,
    outputs: Buses,
    audio: usize,
    key: usize,
}

impl Keyed {
    pub(crate) fn new(width: ChannelLayout) -> Self {
        let main = main_bus(width);
        let inputs = match BusTopology::discrete(width) {
            Some(key) if !main.is_empty() => main
                .with(Bus {
                    topology: key,
                    role: BusRole::Sidechain,
                })
                .unwrap_or(main),
            _ => main,
        };
        let first = |role: BusRole| {
            inputs
                .iter()
                .position(|b| b.role == role)
                .and_then(|i| inputs.first_port(i))
        };
        let n = usize::from(width.count());
        Self {
            width,
            inputs,
            outputs: main,
            audio: first(BusRole::Main).unwrap_or(0),
            key: first(BusRole::Sidechain).unwrap_or(n),
        }
    }

    /// The channels of the audio, the key and the output.
    pub(crate) const fn width(&self) -> ChannelLayout {
        self.width
    }

    /// What the node declares on its input side.
    pub(crate) const fn inputs(&self) -> Buses {
        self.inputs
    }

    /// What the node declares on its output side.
    pub(crate) const fn outputs(&self) -> Buses {
        self.outputs
    }

    /// The input port of audio channel `c`.
    pub(crate) const fn audio(&self, c: usize) -> usize {
        self.audio + c
    }

    /// The input port of key channel `c`.
    pub(crate) const fn key(&self, c: usize) -> usize {
        self.key + c
    }
}
