//! [`Cadence`]: the grid on which a node samples `theta(t)`.

use core::num::NonZeroU32;
use core::ops::Range;

use tutti_types::Samples;

/// When a node samples `theta(t)`: on the block, or on a
/// grid of the stream clock. An engineering choice, not a property of a system,
/// except that it decides whether a render depends on the block length
/// ([`block_independent`](Self::block_independent)).
///
/// Only the two forms a node can have are values: a period in frames counted from
/// the start of each block would be the block's length by another name, and a
/// stream grid with no period is no grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Cadence {
    /// One sample of `theta` per block, at the block: the render depends on how
    /// audio is split.
    Block,
    /// The stream clock, from the last reset: grid points every `period` frames,
    /// at `offset + k * period`, wherever the blocks fall.
    Stream {
        /// The step of the grid, in frames.
        period: NonZeroU32,
        /// The frame of the first grid point.
        offset: Samples,
    },
}

impl Cadence {
    /// One sample of `theta` per block, at the block.
    pub const BLOCK: Self = Self::Block;

    /// Every `n` frames on the stream clock, from frame 0.
    pub const fn stream(n: NonZeroU32) -> Self {
        Self::Stream {
            period: n,
            offset: Samples::ZERO,
        }
    }

    /// The steps of a block of `size` frames that begins at stream frame
    /// `position` (frames since the last reset): the pieces a node renders one at
    /// a time.
    ///
    /// - **[`Block`](Self::Block)** (`position` is not read): one step, solved at
    ///   its **last** frame ([`Step::solve_at`]). `unprimed` makes frame 0 a step
    ///   of its own: a node with nothing to interpolate *from* (its first block
    ///   after `prepare`) solves there exactly.
    /// - **[`Stream`](Self::Stream)**: the block is cut at the grid points of the
    ///   stream clock, `offset + k * period`, wherever the block falls. A step that
    ///   begins on a grid point ([`Step::grid_point`]) is solved at its **first**
    ///   frame and renders toward that solve; the first step of a block that
    ///   begins inside a segment is the rest of a segment whose solve an earlier
    ///   block made, and is not a grid point. `unprimed` is not read: a stream
    ///   starts on a grid point.
    pub fn steps(self, position: u64, size: usize, unprimed: bool) -> impl Iterator<Item = Step> {
        // How far into a period of the grid the block begins.
        let phase = match self {
            Self::Block => 0,
            Self::Stream { period, offset } => {
                let n = u64::from(period.get());
                (position + n - offset.0 as u64 % n) % n
            }
        };
        let mut start = 0;
        core::iter::from_fn(move || {
            if start >= size {
                return None;
            }
            let step = match self {
                Self::Block => {
                    let end = if unprimed && start == 0 { 1 } else { size };
                    Step {
                        frames: start..end,
                        solve_at: end - 1,
                        grid_point: true,
                    }
                }
                Self::Stream { period, .. } => {
                    let period = period.get() as usize;
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

    /// Whether the render is the same whatever the block lengths: a period on the
    /// stream clock. A block cadence is not.
    pub const fn block_independent(self) -> bool {
        matches!(self, Self::Stream { .. })
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
