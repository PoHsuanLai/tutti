//! [`BusTopology`] — a [`ChannelTopology`] small enough to be `Copy`.
//!
//! A node's `Shape` is `Copy` and a [`ChannelTopology`] owns a `Vec`, so the
//! speakers of one bus of a node live inline here, up to the width the engine
//! supports. The two convert both ways; the one direction that can refuse is
//! the wider one.

use crate::{ChannelLayout, ChannelTopology, Speaker};

/// The most channels one bus holds: the widest device the engine folds to.
pub const MAX_BUS_CHANNELS: usize = 16;

/// The speakers of one bus, in channel order, held inline.
///
/// The same list a [`ChannelTopology`] is, bounded by [`MAX_BUS_CHANNELS`] and
/// `Copy`. It may be empty (a bus the plugin declares with no channels), and
/// then a fold from or to it is [`FoldError::Empty`](crate::FoldError::Empty),
/// as for any empty topology.
///
/// `new` refuses a list wider than a bus can hold; it never truncates. The
/// slots past the length are always [`Speaker::Unassigned`], so the derived
/// equality compares the bus and nothing else.
///
/// ```
/// use tutti_types::{BusTopology, ChannelLayout, ChannelTopology, Speaker};
///
/// let stereo = BusTopology::new([Speaker::FrontLeft, Speaker::FrontRight]).unwrap();
/// assert_eq!(stereo.layout(), ChannelLayout::STEREO);
/// assert_eq!(ChannelTopology::from(stereo), ChannelTopology::smpte(ChannelLayout::STEREO).unwrap());
/// assert!(BusTopology::new([Speaker::Unassigned; 17]).is_none());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BusTopology {
    len: u8,
    speakers: [Speaker; MAX_BUS_CHANNELS],
}

/// A topology too wide for one bus, refused by `BusTopology`'s `TryFrom`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BusTooWide {
    /// How many channels the refused list has.
    pub width: usize,
}

impl core::fmt::Display for BusTooWide {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{} channels is wider than a bus ({MAX_BUS_CHANNELS})",
            self.width
        )
    }
}

impl std::error::Error for BusTooWide {}

impl BusTopology {
    /// A bus with no channels.
    pub const EMPTY: Self = Self {
        len: 0,
        speakers: [Speaker::Unassigned; MAX_BUS_CHANNELS],
    };

    /// A stereo pair: front left, front right.
    pub const STEREO: Self = {
        let mut speakers = Self::EMPTY.speakers;
        speakers[0] = Speaker::FrontLeft;
        speakers[1] = Speaker::FrontRight;
        Self { len: 2, speakers }
    };

    /// The bus of these speakers, or `None` past [`MAX_BUS_CHANNELS`].
    pub fn new(positions: impl IntoIterator<Item = Speaker>) -> Option<Self> {
        let mut bus = Self::EMPTY;
        for speaker in positions {
            let at = usize::from(bus.len);
            *bus.speakers.get_mut(at)? = speaker;
            bus.len += 1;
        }
        Some(bus)
    }

    /// `width` channels that feed no speaker, or `None` past
    /// [`MAX_BUS_CHANNELS`]. A width is not a placement: this is the bus a
    /// node that states only a count has.
    #[must_use]
    pub const fn unassigned(width: ChannelLayout) -> Option<Self> {
        if width.count() as usize > MAX_BUS_CHANNELS {
            return None;
        }
        Some(Self {
            len: width.count() as u8,
            speakers: Self::EMPTY.speakers,
        })
    }

    /// The speaker each channel feeds, in channel order.
    #[must_use]
    pub fn positions(&self) -> &[Speaker] {
        &self.speakers[..usize::from(self.len)]
    }

    /// The channel count.
    #[must_use]
    pub const fn layout(&self) -> ChannelLayout {
        ChannelLayout::from_count(self.len as u16)
    }
}

impl From<BusTopology> for ChannelTopology {
    fn from(bus: BusTopology) -> Self {
        Self::new(bus.positions().iter().copied())
    }
}

impl TryFrom<&ChannelTopology> for BusTopology {
    type Error = BusTooWide;

    fn try_from(topology: &ChannelTopology) -> Result<Self, BusTooWide> {
        Self::new(topology.positions().iter().copied()).ok_or(BusTooWide {
            width: topology.positions().len(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `new` and `TryFrom` agree on the edge of the range, a width of 16 is a
    /// bus and 17 is refused with its width, and the round trip through
    /// `ChannelTopology` is the identity.
    #[test]
    fn a_bus_holds_sixteen_channels_and_refuses_more() {
        let sixteen = ChannelTopology::unassigned(ChannelLayout::from(16u16));
        let bus = BusTopology::try_from(&sixteen).unwrap();
        assert_eq!(bus.layout().count(), 16);
        assert_eq!(ChannelTopology::from(bus), sixteen);
        assert_eq!(
            BusTopology::unassigned(ChannelLayout::from(16u16)),
            Some(bus)
        );

        let seventeen = ChannelTopology::unassigned(ChannelLayout::from(17u16));
        assert_eq!(
            BusTopology::try_from(&seventeen),
            Err(BusTooWide { width: 17 })
        );
        assert_eq!(BusTopology::unassigned(ChannelLayout::from(17u16)), None);
    }

    /// Placement survives, and the slots past the length do not leak into
    /// equality: a 2-wide bus built two ways is equal.
    #[test]
    fn speakers_keep_their_slots_and_spare_slots_are_canonical() {
        use Speaker::*;
        let a = BusTopology::new([FrontRight, FrontLeft]).unwrap();
        assert_eq!(a.positions(), &[FrontRight, FrontLeft]);
        let wide = BusTopology::new([FrontRight, FrontLeft, Unassigned]).unwrap();
        assert_ne!(a, wide);
        assert_eq!(BusTopology::new([]).unwrap(), BusTopology::EMPTY);
        assert_eq!(
            BusTopology::unassigned(ChannelLayout::STEREO),
            BusTopology::new([Unassigned, Unassigned])
        );
    }
}
