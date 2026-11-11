//! Which speaker each channel of a decoded track feeds.
//!
//! The one place that reads symphonia's `Channels`. A decoder hands its
//! planar channels in ascending bit order of that set (Vorbis, whose own order
//! is `FL FC FR RL RR LFE`, is reordered by symphonia's `map_vorbis_channel`
//! into it), so the topology lists the speakers in the same order and channel
//! `n` of the samples is entry `n` of the topology.

use symphonia::core::audio::Channels;
use tutti_types::{ChannelTopology, Speaker};

/// The speaker of one symphonia channel flag.
///
/// A flag the vocabulary has no speaker for (the second LFE, the rear
/// left/right of centre, the wide and the high front channels) is
/// [`Speaker::Unknown`] carrying its bit index, which is its slot and no more:
/// it is a position the format named and [`Speaker`] cannot, not a channel
/// known to feed no speaker, so it is not [`Speaker::Discrete`]. A fold that
/// meets one refuses it ([`FoldError::Unnamed`](tutti_types::FoldError::Unnamed))
/// rather than guess where it goes. (The sampler builds that fold when it
/// builds a voice over the wave, so a file with one is refused there.)
fn speaker(flag: Channels) -> Speaker {
    crate::wave_mask::speaker_of(flag.bits())
        .unwrap_or_else(|| Speaker::Unknown(flag.bits().trailing_zeros() as u16))
}

/// The topology of a track's channels, in the order its decoder writes them.
///
/// A lone channel is the engine's mono ([`Speaker::FrontCenter`]): symphonia
/// names the only channel of a mono file `FRONT_LEFT`, and the engine's mono
/// is the centre.
pub(super) fn topology_of(channels: Channels) -> ChannelTopology {
    if channels.count() == 1 {
        return ChannelTopology::new([Speaker::FrontCenter]);
    }
    ChannelTopology::new(channels.iter().map(speaker))
}

/// What a container that states no channels is taken to have: stereo.
pub(super) fn unstated() -> ChannelTopology {
    ChannelTopology::new([Speaker::FrontLeft, Speaker::FrontRight])
}

#[cfg(test)]
mod tests;
