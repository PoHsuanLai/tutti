//! The one vocabulary an event holder reports its losses in.

use std::iter::Sum;
use std::ops::{Add, AddAssign};

/// What an event path lost, in four cumulative counts of **events**.
///
/// Never frames: an audio loss stays a `Samples` (`TapReader::take_lost`,
/// `Recorder::lost`). Every holder of events (the event inlet, the hardware
/// MIDI ports, the ingest node, the clock master) answers one `Losses` from
/// its `losses()`, so a caller sums holders (`Add`, `Sum`) and never learns
/// each one's own getters.
///
/// - [`lost`](Self::lost): gone without a trace, or refused at a full ring on
///   the way in (the inlet's writer answers `Refused`).
/// - [`evicted`](Self::evicted): held, then dropped to make room for another.
///   A full waiting list or a ring that overwrote its oldest.
/// - [`refused`](Self::refused): a port or a node declined it: no room at the
///   destination, or no destination (a Group with no port).
/// - [`late`](Self::late): found past its frame and delivered at the block's
///   first frame. Not a loss of the event, a loss of its time.
///
/// `non_exhaustive`: build one with [`new`](Self::new) or read a holder's.
///
/// ```
/// use tutti_graph::Losses;
///
/// let a = Losses::new(1, 0, 0, 0);
/// let b = Losses::new(0, 2, 3, 4);
/// assert_eq!(a + b, Losses::new(1, 2, 3, 4));
/// assert_eq!([a, b].into_iter().sum::<Losses>().total(), 10);
/// ```
#[non_exhaustive]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Losses {
    /// Events lost: refused at a full ring, or gone without a trace.
    pub lost: u64,
    /// Events dropped from a waiting list or overwritten in a ring for room.
    pub evicted: u64,
    /// Events a port or node refused, or that had no port to go to.
    pub refused: u64,
    /// Events delivered after their frame.
    pub late: u64,
}

impl Losses {
    /// No losses.
    pub const NONE: Self = Self::new(0, 0, 0, 0);

    /// The four counts, in the order of the fields.
    #[must_use]
    pub const fn new(lost: u64, evicted: u64, refused: u64, late: u64) -> Self {
        Self {
            lost,
            evicted,
            refused,
            late,
        }
    }

    /// Events that never arrived: `lost + evicted + refused`. A late event
    /// arrived, so it is not in it.
    #[must_use]
    pub const fn dropped(self) -> u64 {
        self.lost
            .saturating_add(self.evicted)
            .saturating_add(self.refused)
    }

    /// Every count summed, late included.
    #[must_use]
    pub const fn total(self) -> u64 {
        self.dropped().saturating_add(self.late)
    }
}

impl Add for Losses {
    type Output = Self;

    fn add(self, rhs: Self) -> Self {
        Self::new(
            self.lost.saturating_add(rhs.lost),
            self.evicted.saturating_add(rhs.evicted),
            self.refused.saturating_add(rhs.refused),
            self.late.saturating_add(rhs.late),
        )
    }
}

impl AddAssign for Losses {
    fn add_assign(&mut self, rhs: Self) {
        *self = *self + rhs;
    }
}

impl Sum for Losses {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::NONE, Add::add)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each field adds to its own field, and `dropped` leaves `late` out.
    #[test]
    fn addition_is_field_wise_and_dropped_excludes_late() {
        let sum = Losses::new(1, 2, 3, 4) + Losses::new(10, 20, 30, 40);
        assert_eq!(sum, Losses::new(11, 22, 33, 44));
        assert_eq!(sum.dropped(), 66);
        assert_eq!(sum.total(), 110);
        let mut acc = Losses::NONE;
        acc += Losses::new(0, 0, 5, 0);
        assert_eq!(acc.refused, 5);
        assert_eq!(
            [Losses::new(1, 0, 0, 0), Losses::new(0, 0, 0, 1)]
                .into_iter()
                .sum::<Losses>(),
            Losses::new(1, 0, 0, 1)
        );
    }
}
