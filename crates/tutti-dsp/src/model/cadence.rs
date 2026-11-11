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

    /// The steps of a block of `size` frames that begins at stream frame
    /// `position` (frames since the last reset): the pieces a node renders one at
    /// a time.
    ///
    /// - **Block anchor** (`position` is not read): a [`Period::Block`] grid is one
    ///   step and [`every(n)`](Self::every) tiles the block in steps of at most `n`
    ///   frames, each solved at its **last** frame ([`Step::solve_at`]).
    ///   `unprimed` makes frame 0 a step of its own: a node with nothing to
    ///   interpolate *from* (its first block after `prepare`) solves there exactly.
    /// - **Stream anchor**: the block is cut at the grid points of the stream
    ///   clock, `offset + k * period`, wherever the block falls. A step that
    ///   begins on a grid point ([`Step::grid_point`]) is solved at its **first**
    ///   frame and renders toward that solve; the first step of a block that
    ///   begins inside a segment is the rest of a segment whose solve an earlier
    ///   block made, and is not a grid point. `unprimed` is not read: a stream
    ///   starts on a grid point.
    pub fn steps(self, position: u64, size: usize, unprimed: bool) -> impl Iterator<Item = Step> {
        let period = match self.period {
            Period::Block => size.max(1),
            Period::Frames(n) => n.get() as usize,
        };
        let anchor = self.anchor;
        let phase = match anchor {
            Anchor::Block => 0,
            Anchor::Stream { offset } => {
                let n = period as u64;
                (position + n - offset.0 as u64 % n) % n
            }
        };
        let mut start = 0;
        core::iter::from_fn(move || {
            if start >= size {
                return None;
            }
            let step = match anchor {
                Anchor::Block => {
                    let end = if unprimed && start == 0 {
                        1
                    } else {
                        (start + period).min(size)
                    };
                    Step {
                        frames: start..end,
                        solve_at: end - 1,
                        grid_point: true,
                    }
                }
                Anchor::Stream { .. } => {
                    let into = ((phase + start as u64) % period as u64) as usize;
                    Step {
                        frames: start..(start + period - into).min(size),
                        solve_at: start,
                        grid_point: into == 0,
                    }
                }
            };
            start = step.frames.end;
            Some(step)
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
    /// Whether the step begins a segment of the grid, so `theta` is sampled for it
    /// here. Always true on a block grid; false on a stream grid for the rest of
    /// a segment that an earlier block began.
    pub grid_point: bool,
}

#[cfg(test)]
mod tests;
