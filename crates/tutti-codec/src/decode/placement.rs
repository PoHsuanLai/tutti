//! Which speaker each channel of a decoded track feeds.
//!
//! The one place that reads symphonia's `Channels`. A decoder hands its
//! planar channels in ascending bit order of a positioned set (Vorbis, whose own
//! order is `FL FC FR RL RR LFE`, is reordered by symphonia into it), so the
//! topology lists the speakers in the same order and channel `n` of the samples
//! is entry `n` of the topology.

use symphonia::core::audio::{Channels, Position};
use tutti_types::{ChannelTopology, Speaker};

/// The speaker of one symphonia position flag.
///
/// A flag the vocabulary has no speaker for (the second LFE, the wide and the
/// high side channels) is [`Speaker::Unknown`] carrying its bit index, which is
/// its slot and no more: it is a position the format named and [`Speaker`]
/// cannot, not a channel known to feed no speaker, so it is not
/// [`Speaker::Unassigned`]. A fold that meets one refuses it
/// ([`FoldError::Unnamed`](tutti_types::FoldError::Unnamed)) rather than guess
/// where it goes. (The sampler builds that fold when it builds a voice over the
/// wave, so a file with one is refused there.)
fn speaker(flag: Position) -> Speaker {
    let bits = flag.bits();
    u32::try_from(bits)
        .ok()
        .and_then(crate::wave_mask::speaker_of)
        .unwrap_or_else(|| Speaker::Unknown(bits.trailing_zeros() as u16))
}

/// The topology of a track's channels, in the order its decoder writes them.
///
/// A lone channel is the engine's mono ([`Speaker::FrontCenter`]): symphonia
/// names the only channel of a mono file `FRONT_LEFT`, and the engine's mono
/// is the centre. Channels that name no position (discrete, ambisonic or
/// labelled ones) feed no speaker: each is [`Speaker::Unassigned`].
pub(super) fn topology_of(channels: &Channels) -> ChannelTopology {
    if channels.count() == 1 {
        return ChannelTopology::new([Speaker::FrontCenter]);
    }
    match channels {
        Channels::Positioned(positions) => ChannelTopology::new(positions.iter().map(speaker)),
        other => ChannelTopology::new(std::iter::repeat_n(Speaker::Unassigned, other.count())),
    }
}

/// What a container that states no channels is taken to have: stereo.
pub(super) fn unstated() -> ChannelTopology {
    ChannelTopology::new([Speaker::FrontLeft, Speaker::FrontRight])
}

#[cfg(test)]
mod tests;
