//! [`Samples`] — a count of audio frames.
//!
//! A discrete count, distinct from the float-backed *measure* units (`Hz`,
//! `Seconds`, `Beat`, `SamplePosition`, …) in [`super::units`]. Those are
//! continuous quantities you interpolate and feed to [`Param`](super::Param);
//! this is an integer you compare and add.
//!
//! Deliberately **not** implementing the [`Unit`](super::Unit) marker trait — a
//! compensation delay is not an automatable DSP parameter and must not be
//! reachable through [`Param`](super::Param). The practical difference shows up
//! in the derives: `Eq + Ord + Hash`, which counts need for map keys and
//! `max()`, and which the float units cannot have.

/// A count of audio frames: reported latency, compensation delay, ring length.
///
/// Integer by construction. Distinct from `SamplePosition`, which is an `f64`
/// *position* within a wave that may be fractional so interpolating readers can
/// address between two integer sample indices.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Samples(pub usize);

impl From<usize> for Samples {
    #[inline]
    fn from(v: usize) -> Self {
        Self(v)
    }
}

impl From<Samples> for usize {
    #[inline]
    fn from(v: Samples) -> usize {
        v.0
    }
}

impl core::fmt::Display for Samples {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(&self.0, f)
    }
}

impl Samples {
    #[inline]
    pub const fn new(v: usize) -> Self {
        Self(v)
    }

    #[inline]
    pub const fn get(self) -> usize {
        self.0
    }

    /// The delay needed so a signal arriving at `self` lines up with one
    /// arriving at `target`.
    ///
    /// Saturates: a signal already later than `target` needs no delay, and
    /// never a negative one.
    #[inline]
    pub const fn align_to(self, target: Samples) -> Samples {
        Samples(target.0.saturating_sub(self.0))
    }

    #[inline]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align_to_yields_the_gap() {
        assert_eq!(Samples(100).align_to(Samples(512)), Samples(412));
        assert_eq!(Samples(512).align_to(Samples(512)), Samples(0));
    }

    #[test]
    fn align_to_saturates_when_already_late() {
        // A signal arriving later than the target needs no delay.
        assert_eq!(Samples(900).align_to(Samples(512)), Samples(0));
    }

    #[test]
    fn counts_order_and_round_trip() {
        assert!(Samples(100) < Samples(512));
        assert_eq!(
            [Samples(3), Samples(9), Samples(1)].iter().max(),
            Some(&Samples(9))
        );
        assert_eq!(usize::from(Samples::new(2)), 2);
        assert_eq!(Samples::from(1).get(), 1);
    }
}
