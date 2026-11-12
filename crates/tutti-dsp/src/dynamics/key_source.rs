//! [`KeySource`]: where a keyed dynamics family's detector listens.

use tutti_types::BusTopology;

use crate::{Channels, Feed};

/// The signal a compressor's or a gate's detector reads: a type of the family, so a
/// graph's port layout and its sound say the same thing.
///
/// - [`Sidechain`](Self::Sidechain): a second bus of the node, as wide as the signal
///   ([`Channels::keyed`]). The detector measures the key and the gain is applied to the
///   signal, so the signal being measured need not be the signal being shaped (a ducker,
///   a de-esser, a gate keyed from a kick). **A key nobody feeds is silence**, which
///   compresses nothing and holds a gate closed: the graph feeds an unconnected input
///   zeros, and a node cannot tell a wired key from an unwired one.
/// - [`Own`](Self::Own): no key bus ([`Channels::same`]). The detector reads the signal
///   itself, as an ordinary compressor or gate does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeySource {
    /// The detector reads a side-chain bus, as wide as the signal.
    Sidechain,
    /// The detector reads the signal itself; the family has no key bus.
    Own,
}

impl KeySource {
    /// The channels of a family on `input` with this key: audio and a key bus of the same
    /// width for a [`Sidechain`](Self::Sidechain), the audio alone for [`Own`](Self::Own).
    #[must_use]
    pub const fn channels(self, input: BusTopology) -> Channels {
        match self {
            Self::Sidechain => Channels::keyed(input),
            Self::Own => Channels::same(input),
        }
    }

    /// The channels a detector reads in `feed`: its key, or its audio.
    #[inline]
    pub(crate) fn detected<'a>(self, feed: &Feed<'a>) -> &'a [&'a [f32]] {
        match self {
            Self::Sidechain => feed.key,
            Self::Own => feed.audio,
        }
    }
}
