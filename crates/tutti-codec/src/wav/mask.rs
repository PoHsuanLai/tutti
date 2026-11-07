//! The speaker mask a multichannel WAV file declares.
//!
//! A WAVE file stores channels in ascending speaker-bit order, so a mask can
//! describe a [`ChannelTopology`] only when its speakers already are in that
//! order. [`channel_mask`] returns the mask of such a topology, and `0` (the
//! "unassigned" mask: the channels are anonymous rather than mislabelled) when
//! a speaker has no WAVE bit or the order differs. Samples are never
//! reordered, so file channel `n` is always the engine's channel `n`.
//!
//! A writer that is given only a width uses [`default_channel_mask`], which is
//! [`channel_mask`] of [`ChannelTopology::smpte`], so it never guesses where
//! the engine declines to: 1 channel is `0x4` (`KSAUDIO_SPEAKER_MONO`), 2 is
//! `0x3`, 6 is `0x60F` (`KSAUDIO_SPEAKER_5POINT1_SURROUND`), and every other
//! width is `0`, including 8 (tutti's 7.1 is `... SL SR BL BR`, WAVE's would be
//! `... BL BR SL SR`) and 4 (quad and B-format are both four channels).
//! Quad's `0x33` (`KSAUDIO_SPEAKER_QUAD`) needs the caller to pass
//! [`ChannelTopology::quad`].

use tutti_types::{ChannelLayout, ChannelTopology, Speaker};

/// The `SPEAKER_*` bit of `speaker` in `ksmedia.h`, or `None` for a speaker
/// WAVE has no bit for.
const fn speaker_bit(speaker: Speaker) -> Option<u32> {
    Some(match speaker {
        Speaker::FrontLeft => 0x1,
        Speaker::FrontRight => 0x2,
        Speaker::FrontCenter => 0x4,
        Speaker::LowFrequency => 0x8,
        Speaker::BackLeft => 0x10,
        Speaker::BackRight => 0x20,
        Speaker::FrontLeftOfCenter => 0x40,
        Speaker::FrontRightOfCenter => 0x80,
        Speaker::BackCenter => 0x100,
        Speaker::SideLeft => 0x200,
        Speaker::SideRight => 0x400,
        Speaker::TopCenter => 0x800,
        Speaker::TopFrontLeft => 0x1000,
        Speaker::TopFrontCenter => 0x2000,
        Speaker::TopFrontRight => 0x4000,
        Speaker::TopBackLeft => 0x8000,
        Speaker::TopBackCenter => 0x10000,
        Speaker::TopBackRight => 0x20000,
        _ => return None,
    })
}

/// The speaker mask a WAV file declares for `topology`, or `0` (unassigned)
/// when a speaker has no WAVE bit or the speakers are not in ascending bit
/// order, which is the only order a WAVE file can state.
pub(crate) fn channel_mask(topology: &ChannelTopology) -> u32 {
    let mut mask = 0u32;
    for &speaker in topology.positions() {
        match speaker_bit(speaker) {
            Some(bit) if bit > mask => mask |= bit,
            _ => return 0,
        }
    }
    mask
}

/// [`channel_mask`] of the engine's own order for a bare width
/// ([`ChannelTopology::smpte`]), `0` for a width that has none.
#[must_use]
pub(crate) fn default_channel_mask(layout: ChannelLayout) -> u32 {
    ChannelTopology::smpte(layout).map_or(0, |t| channel_mask(&t))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The masks are the `ksmedia.h` constants. Quad comes only from a quad
    /// topology; a bare width of 4, 7.1 and unnamed widths are unassigned, and
    /// so are a topology out of ascending order or with a speaker WAVE has no
    /// bit for.
    ///
    /// Mutation: giving `default_channel_mask` a `4 => 0x33` arm (the count
    /// guess) fails the bare-4 assertion; making `channel_mask` skip the
    /// ascending check gives the swapped pair `0x3`, failing that assertion;
    /// SideLeft mapped to BackLeft's bit gives `0x3F` for 5.1.
    #[test]
    fn masks_match_ksmedia_and_are_zero_where_they_cannot_be_stated() {
        let m = |n: u16| default_channel_mask(ChannelLayout::from(n));
        assert_eq!(m(1), 0x4);
        assert_eq!(m(2), 0x3);
        assert_eq!(m(6), 0x60F);
        assert_eq!(m(4), 0, "a bare 4 may be quad or B-format");
        assert_eq!(
            m(8),
            0,
            "tutti's 7.1 is SL SR BL BR, WAVE's would be BL BR SL SR"
        );
        assert_eq!(m(3), 0);
        assert_eq!(m(12), 0);
        assert_eq!(channel_mask(&ChannelTopology::quad()), 0x33);
        assert_eq!(
            channel_mask(&ChannelTopology::smpte(ChannelLayout::from(6u16)).unwrap()),
            0x60F
        );
        use Speaker::*;
        assert_eq!(
            channel_mask(&ChannelTopology::new([FrontRight, FrontLeft])),
            0
        );
        assert_eq!(
            channel_mask(&ChannelTopology::new([FrontLeft, TopSideLeft])),
            0
        );
        assert_eq!(
            channel_mask(&ChannelTopology::new([FrontLeft, Unknown(3)])),
            0
        );
        assert_eq!(
            channel_mask(&ChannelTopology::new([FrontLeft, FrontLeft])),
            0
        );
    }
}
