//! [`Buses`]: how a node's audio ports group into buses.
//!
//! A [`Shape`](crate::Shape)'s `audio_in` and `audio_out` count channels, and
//! each channel is a port. A bus is a **grouping over those ports** that says
//! where its channels sit: bus `b` is the run of ports after the earlier
//! buses' (doc 013, "Buses and coercion between declared layouts"). Nothing
//! about a port changes: a bus is what a fold between two declared layouts
//! will be built from, and nothing the compiler or the executor reads yet.

use tutti_types::{BusTopology, ChannelLayout};

/// Most buses declared per side of one node: a main, a sidechain and two aux.
///
/// A node with more declares its first four; the rest are reachable by port.
pub const MAX_BUSES: usize = 4;

/// What a bus is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BusRole {
    /// The bus a node's signal path runs through.
    Main,
    /// An input the node reads to steer itself (a compressor's key), not to
    /// pass through.
    Sidechain,
    /// Any other bus: a plugin's extra outputs, or an input the format does
    /// not say is a sidechain.
    Aux,
}

/// One bus: where its channels sit, and what it is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Bus {
    /// The speaker each channel feeds, in channel order.
    pub topology: BusTopology,
    /// What the bus is for.
    pub role: BusRole,
}

impl Bus {
    const NONE: Self = Self {
        topology: BusTopology::EMPTY,
        role: BusRole::Aux,
    };

    /// The channel count.
    #[must_use]
    pub const fn width(&self) -> usize {
        self.topology.layout().count() as usize
    }
}

/// The buses of one side (input or output) of a node, in port order: up to
/// [`MAX_BUSES`], held inline so a [`Shape`](crate::Shape) stays `Copy`.
///
/// They cover a **prefix** of the side's ports, so
/// [`width`](Self::width) is at most the side's channel count. Equal to it for
/// a node whose every channel is in a bus (every node but a plugin with more
/// than [`MAX_BUSES`] buses, or a bus wider than
/// [`MAX_BUS_CHANNELS`](tutti_types::MAX_BUS_CHANNELS)): the ports past the
/// buses are reached by port, not by bus.
///
/// ```
/// use tutti_graph::{Bus, BusRole, Buses};
/// use tutti_types::{BusTopology, ChannelLayout, Speaker};
///
/// let stereo = BusTopology::new([Speaker::FrontLeft, Speaker::FrontRight]).unwrap();
/// let key = BusTopology::discrete(ChannelLayout::MONO).unwrap();
/// let buses = Buses::NONE
///     .with(Bus { topology: stereo, role: BusRole::Main }).unwrap()
///     .with(Bus { topology: key, role: BusRole::Sidechain }).unwrap();
/// assert_eq!(buses.width(), 3);
/// assert_eq!(buses.first_port(1), Some(2));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Buses {
    len: u8,
    buses: [Bus; MAX_BUSES],
}

impl Buses {
    /// No buses: a node with no audio on this side, or one whose channels are
    /// reached by port alone.
    pub const NONE: Self = Self {
        len: 0,
        buses: [Bus::NONE; MAX_BUSES],
    };

    /// One [`Main`](BusRole::Main) bus of `topology`.
    #[must_use]
    pub const fn main(topology: BusTopology) -> Self {
        let mut buses = Self::NONE;
        buses.buses[0] = Bus {
            topology,
            role: BusRole::Main,
        };
        buses.len = 1;
        buses
    }

    /// One [`Main`](BusRole::Main) bus of `width` channels that name no
    /// speaker ([`Discrete`](tutti_types::Speaker::Discrete)): unplaced, said
    /// so. [`NONE`](Self::NONE) for a width of 0.
    ///
    /// # Panics
    ///
    /// Past [`MAX_BUS_CHANNELS`](tutti_types::MAX_BUS_CHANNELS) (in a `const`
    /// context, at compile time): a wider side is
    /// [`Shape::through_discrete`](crate::Shape::through_discrete) and its
    /// kin, or several buses.
    #[must_use]
    pub const fn discrete(width: ChannelLayout) -> Self {
        assert!(
            width.count() as usize <= tutti_types::MAX_BUS_CHANNELS,
            "a discrete bus wider than a bus holds"
        );
        Self::unplaced(width)
    }

    /// [`discrete`](Self::discrete), or [`NONE`](Self::NONE) past a bus's
    /// width, whose channels are then ports with no bus.
    pub(crate) const fn unplaced(width: ChannelLayout) -> Self {
        match BusTopology::discrete(width) {
            Some(topology) if width.count() > 0 => Self::main(topology),
            _ => Self::NONE,
        }
    }

    /// These buses and one more after them, or `None` past [`MAX_BUSES`].
    #[must_use]
    pub const fn with(mut self, bus: Bus) -> Option<Self> {
        if self.len as usize == MAX_BUSES {
            return None;
        }
        self.buses[self.len as usize] = bus;
        self.len += 1;
        Some(self)
    }

    /// The buses of `list`, or `None` past [`MAX_BUSES`].
    #[must_use]
    pub fn new(list: &[Bus]) -> Option<Self> {
        list.iter()
            .try_fold(Self::NONE, |buses, &bus| buses.with(bus))
    }

    /// How many buses.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len as usize
    }

    /// Whether there are none.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Bus `index`, in port order.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&Bus> {
        self.as_slice().get(index)
    }

    /// The buses, in port order.
    #[must_use]
    pub fn as_slice(&self) -> &[Bus] {
        &self.buses[..usize::from(self.len)]
    }

    /// The buses, in port order.
    pub fn iter(&self) -> core::slice::Iter<'_, Bus> {
        self.as_slice().iter()
    }

    /// The first [`Main`](BusRole::Main) bus and its index.
    #[must_use]
    pub fn main_bus(&self) -> Option<(usize, &Bus)> {
        self.iter()
            .enumerate()
            .find(|(_, b)| b.role == BusRole::Main)
    }

    /// The channels the buses hold between them: the ports they cover.
    #[must_use]
    pub const fn width(&self) -> usize {
        let mut total = 0;
        let mut i = 0;
        while i < self.len as usize {
            total += self.buses[i].width();
            i += 1;
        }
        total
    }

    /// The port bus `index`'s first channel is, on the side these buses
    /// describe: the widths of the buses before it.
    #[must_use]
    pub fn first_port(&self, index: usize) -> Option<usize> {
        (index < self.len()).then(|| self.as_slice()[..index].iter().map(Bus::width).sum())
    }
}

impl<'a> IntoIterator for &'a Buses {
    type Item = &'a Bus;
    type IntoIter = core::slice::Iter<'a, Bus>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}
