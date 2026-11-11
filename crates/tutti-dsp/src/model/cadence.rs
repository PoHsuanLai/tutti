//! [`Cadence`]: the grid on which a node samples `theta(t)`.

use core::num::NonZeroU32;
use core::ops::Range;

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

    /// The steps of a block of `size` frames on a block-anchored grid: the
    /// pieces a node designs and renders one at a time. A [`Period::Block`] grid
    /// is one step; [`every(n)`](Self::every) tiles the block in steps of at most
    /// `n` frames. Each step is solved at its **last** frame
    /// ([`Step::solve_at`]), and the steps before it interpolate from the
    /// previous solve.
    ///
    /// `unprimed` makes frame 0 a step of its own: a node with nothing to
    /// interpolate *from* (its first block after `prepare`) solves there
    /// exactly instead of ramping in from a stand-in.
    ///
    /// # Panics
    ///
    /// On a stream grid: its steps depend on the stream's position, which no
    /// node passes yet (doc 024 Q27).
    pub fn steps(self, size: usize, unprimed: bool) -> impl Iterator<Item = Step> {
        assert!(
            matches!(self.anchor, Anchor::Block),
            "a stream grid's steps need the stream position"
        );
        let first = usize::from(unprimed && size > 0);
        let period = match self.period {
            Period::Block => size.max(1),
            Period::Frames(n) => n.get() as usize,
        };
        core::iter::once(0..1)
            .take(first)
            .chain(
                (first..size)
                    .step_by(period)
                    .map(move |start| start..(start + period).min(size)),
            )
            .map(|frames| Step {
                solve_at: frames.end - 1,
                frames,
            })
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

/// One piece of a block ([`Cadence::steps`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    /// The frames of the block the step renders.
    pub frames: Range<usize>,
    /// The frame at which `theta` is sampled for the step.
    pub solve_at: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ranges(grid: Cadence, size: usize, unprimed: bool) -> Vec<Range<usize>> {
        grid.steps(size, unprimed).map(|s| s.frames).collect()
    }

    /// The steps tile the block with no gap and no overlap, each solved at its
    /// last frame, and a block grid is one step.
    ///
    /// Mutation: solving at `start` instead of `end - 1` fails the `solve_at`
    /// check.
    #[test]
    fn steps_tile_the_block_and_solve_at_their_last_frame() {
        let grid = Cadence::every(N16);
        assert_eq!(ranges(grid, 40, false), vec![0..16, 16..32, 32..40]);
        assert_eq!(ranges(grid, 1, false), vec![0..1]);
        assert_eq!(ranges(grid, 0, false), Vec::<Range<usize>>::new());
        assert_eq!(ranges(Cadence::BLOCK, 40, false), vec![0..40]);
        assert_eq!(ranges(Cadence::BLOCK, 0, false), Vec::<Range<usize>>::new());
        assert!(
            grid.steps(40, false)
                .all(|s| s.solve_at == s.frames.end - 1)
        );
    }

    /// An unprimed block solves its first frame alone, then tiles the rest.
    ///
    /// Mutation: dropping the `take(first)` stage (never isolating frame 0)
    /// fails the first assertion.
    #[test]
    fn an_unprimed_block_isolates_frame_zero() {
        let grid = Cadence::every(N16);
        assert_eq!(ranges(grid, 40, true), vec![0..1, 1..17, 17..33, 33..40]);
        assert_eq!(ranges(grid, 1, true), vec![0..1]);
        assert_eq!(ranges(grid, 0, true), Vec::<Range<usize>>::new());
        assert_eq!(ranges(Cadence::BLOCK, 40, true), vec![0..1, 1..40]);
    }

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
