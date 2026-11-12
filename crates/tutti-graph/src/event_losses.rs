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
/// `non_exhaustive`: build one count at a time ([`Losses::lost`] and its
/// kin, added together) or read a holder's.
///
/// ```
/// use tutti_graph::Losses;
///
/// let a = Losses::lost(1);
/// let b = Losses::evicted(2) + Losses::refused(3) + Losses::late(4);
/// assert_eq!((a + b).dropped(), 6);
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
    pub const NONE: Self = Self {
        lost: 0,
        evicted: 0,
        refused: 0,
        late: 0,
    };

    /// `n` events [lost](Self::lost) and nothing else. A holder with several
    /// counts adds one constructor per count, so no count is placed by
    /// position.
    #[must_use]
    pub const fn lost(n: u64) -> Self {
        Self {
            lost: n,
            ..Self::NONE
        }
    }

    /// `n` events [evicted](Self::evicted) and nothing else.
    #[must_use]
    pub const fn evicted(n: u64) -> Self {
        Self {
            evicted: n,
            ..Self::NONE
        }
    }

    /// `n` events [refused](Self::refused) and nothing else.
    #[must_use]
    pub const fn refused(n: u64) -> Self {
        Self {
            refused: n,
            ..Self::NONE
        }
    }

    /// `n` events [late](Self::late) and nothing else.
    #[must_use]
    pub const fn late(n: u64) -> Self {
        Self {
            late: n,
            ..Self::NONE
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
        Self {
            lost: self.lost.saturating_add(rhs.lost),
            evicted: self.evicted.saturating_add(rhs.evicted),
            refused: self.refused.saturating_add(rhs.refused),
            late: self.late.saturating_add(rhs.late),
        }
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
        let four = |l, e, r, t| Losses {
            lost: l,
            evicted: e,
            refused: r,
            late: t,
        };
        let sum = four(1, 2, 3, 4) + four(10, 20, 30, 40);
        assert_eq!(sum, four(11, 22, 33, 44));
        assert_eq!(sum.dropped(), 66);
        assert_eq!(sum.total(), 110);
        let mut acc = Losses::NONE;
        acc += Losses::refused(5);
        assert_eq!(acc.refused, 5);
        assert_eq!(
            [Losses::lost(1), Losses::late(1)]
                .into_iter()
                .sum::<Losses>(),
            Losses {
                lost: 1,
                late: 1,
                ..Losses::NONE
            }
        );
    }
}
