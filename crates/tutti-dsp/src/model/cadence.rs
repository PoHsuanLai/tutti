//! [`Cadence`]: the grid on which a node samples `theta(t)`.

use core::num::NonZeroU32;

use tutti_types::Samples;

/// How long a step of the grid is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Period {
    /// The whole block: one sample of `theta` per block.
    Block,
    /// A fixed number of frames.
    Frames(NonZeroU32),
}

/// What the grid is counted from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Anchor {
    /// The start of each block: the render depends on how audio is split.
    Block,
    /// The stream clock, from the last reset: grid points at
    /// `offset + k * period`, wherever the blocks fall.
    Stream {
        /// The frame of the first grid point.
        offset: Samples,
    },
}

/// When a node samples `theta(t)` (design 024 section 4.5): a [`Period`] and
/// an [`Anchor`]. An engineering choice, not a property of a system, except
/// that the anchor decides whether a render depends on the block length
/// ([`block_independent`](Self::block_independent)).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Cadence {
    period: Period,
    anchor: Anchor,
}

impl Cadence {
    /// One sample of `theta` per block, at the block.
    pub const BLOCK: Self = Self {
        period: Period::Block,
        anchor: Anchor::Block,
    };

    /// Every `n` frames from the start of each block.
    pub const fn every(n: NonZeroU32) -> Self {
        Self {
            period: Period::Frames(n),
            anchor: Anchor::Block,
        }
    }

    /// Every `n` frames on the stream clock, from frame 0.
    pub const fn stream(n: NonZeroU32) -> Self {
        Self {
            period: Period::Frames(n),
            anchor: Anchor::Stream {
                offset: Samples::ZERO,
            },
        }
    }

    /// The step of the grid.
    pub const fn period(self) -> Period {
        self.period
    }

    /// What the grid is counted from.
    pub const fn anchor(self) -> Anchor {
        self.anchor
    }

    /// Whether the render is the same whatever the block lengths: a fixed
    /// period on the stream clock. A block-anchored cadence is stated as not.
    pub const fn block_independent(self) -> bool {
        matches!(
            (self.period, self.anchor),
            (Period::Frames(_), Anchor::Stream { .. })
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const N16: NonZeroU32 = NonZeroU32::new(16).unwrap();

    /// Only a fixed period on the stream clock is block independent.
    ///
    /// Mutation: `block_independent` that tests the anchor alone (a
    /// `Period::Block` on a stream anchor answers true) or the period alone
    /// (`every(16)` answers true) fails.
    #[test]
    fn only_a_stream_grid_is_block_independent() {
        assert!(!Cadence::BLOCK.block_independent());
        assert!(!Cadence::every(N16).block_independent());
        assert!(Cadence::stream(N16).block_independent());
        let block_on_stream = Cadence {
            period: Period::Block,
            anchor: Anchor::Stream {
                offset: Samples::ZERO,
            },
        };
        assert!(!block_on_stream.block_independent());
    }

    /// The constructors state the period and the anchor the doc names.
    #[test]
    fn the_constructors_name_their_grid() {
        assert_eq!(Cadence::BLOCK.period(), Period::Block);
        assert_eq!(Cadence::every(N16).period(), Period::Frames(N16));
        assert_eq!(Cadence::every(N16).anchor(), Anchor::Block);
        assert_eq!(
            Cadence::stream(N16).anchor(),
            Anchor::Stream {
                offset: Samples::ZERO
            }
        );
    }
}
