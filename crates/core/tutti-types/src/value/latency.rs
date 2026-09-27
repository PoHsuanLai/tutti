//! [`Latency`] — **processing** latency, as a type distinct from a frame count.
//!
//! # Why a newtype over `Samples`
//!
//! A node's *musical* delay (an echo's delay time, a chorus's modulation delay)
//! and its *processing* latency are both a number of frames, and mixing them up
//! is costly: report a 500 ms echo's delay time as its latency and delay
//! compensation drags every parallel path 500 ms late.
//!
//! With `Latency` the two are different types. A delay time stays `Seconds` or
//! `Samples`; a node's declared latency is a `Latency`, built only by
//! [`Latency::new`] — a named, greppable act. Passing a delay time where a
//! latency is expected is a compile error:
//!
//! ```compile_fail
//! use tutti_types::{Latency, Samples};
//! fn declare(_: Latency) {}
//! let echo_time = Samples(24_000);
//! declare(echo_time); // a delay time is not a latency
//! ```
//!
//! `tutti-graph`'s node `Shape` declares its latency as a `Latency`. The
//! graph-value folds in this crate ([`NodeSpec::latency`](crate::graph::NodeSpec::latency)
//! and [`LatencyGraph::latency`](crate::LatencyGraph::latency)) carry the same
//! quantity as a plain [`Samples`].
//!
//! # Algebra
//!
//! Latencies **cascade**: a signal through two latent nodes is late by the sum
//! (`Add`). Merging paths takes the worst (`Ord`, so `max`). There is no `Sub`:
//! the difference of two latencies is not a latency, it is the *compensation
//! delay* one path needs — a frame count, returned by
//! [`gap_to`](Latency::gap_to) as `Samples`.

use super::samples::Samples;

/// The frames a node buffers as a side effect of processing: lookahead, an FFT
/// block, a plugin's pipeline. Never a musical delay.
///
/// See the [module docs](self) for why this is not a bare
/// [`Samples`].
///
/// # Examples
///
/// ```
/// use tutti_types::{Latency, Samples};
///
/// let lookahead = Latency::new(Samples(128));
/// let fft = Latency::new(Samples(64));
/// assert_eq!(lookahead + fft, Latency::new(Samples(192))); // cascade
/// assert_eq!(fft.gap_to(lookahead), Samples(64)); // compensation delay
/// ```
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct Latency(Samples);

impl Latency {
    /// No latency.
    pub const ZERO: Latency = Latency(Samples::ZERO);

    /// Declares `frames` of processing latency.
    ///
    /// Deliberately the only constructor, and deliberately not a `From`: the
    /// point of the type is that becoming a latency is a visible decision.
    #[inline]
    pub const fn new(frames: Samples) -> Self {
        Self(frames)
    }

    /// Returns the latency as a frame count.
    #[inline]
    pub const fn samples(self) -> Samples {
        self.0
    }

    /// Returns whether there is no latency.
    #[inline]
    pub const fn is_zero(self) -> bool {
        self.0.is_zero()
    }

    /// Returns the compensation delay a path arriving `self` late needs to
    /// line up with one arriving `later` late.
    ///
    /// Zero when `self` is already the later.
    #[inline]
    pub const fn gap_to(self, later: Latency) -> Samples {
        self.0.align_to(later.0)
    }
}

/// Latency through a cascade is the sum. Saturating, as `Samples` is.
impl core::ops::Add for Latency {
    type Output = Self;
    #[inline]
    fn add(self, rhs: Self) -> Self {
        Self(self.0 + rhs.0)
    }
}

impl core::fmt::Display for Latency {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} frames latency", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cascade adds, merge takes the max, and the gap between two is a frame
    /// count that saturates.
    ///
    /// Mutation: make `gap_to` return `later.0.align_to(self.0)` (arguments
    /// swapped) → the early path gets zero compensation → fails.
    #[test]
    fn latencies_cascade_merge_and_compensate() {
        let a = Latency::new(Samples(128));
        let b = Latency::new(Samples(64));
        assert_eq!(a + b, Latency::new(Samples(192)));
        assert_eq!(a.max(b), a);
        assert_eq!(b.gap_to(a), Samples(64));
        assert_eq!(a.gap_to(b), Samples::ZERO);
        assert!(Latency::ZERO.is_zero());
    }
}
