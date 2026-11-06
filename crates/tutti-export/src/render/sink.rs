//! The pre-sink gate.
//!
//! One value: [`BlockCursor`], which answers "of this block, which frames does
//! the output keep?" — latency trim at the head, output-length cap at the tail.
//!
//! It is applied by the driver rather than by a sink, because it needs counters
//! that span blocks. Keeping it off the consumer means an encoder only ever sees
//! frames it should write.

use std::ops::Range;
use tutti_types::Samples;

/// Per-block positioning handed from the driver to its gate.
///
/// Width-agnostic: it windows by frame index and never looks inside a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BlockCursor {
    /// Absolute index of the block's first frame.
    pub block_start: Samples,
    /// Leading frames to drop (look-ahead latency).
    pub latency: Samples,
    /// Frames already kept by earlier blocks.
    pub kept_so_far: Samples,
    /// Total frames the output may keep.
    pub output_length: Samples,
}

impl BlockCursor {
    /// The span of this block the output keeps. Empty when the block lies
    /// entirely before the latency gate or entirely past the cap.
    ///
    /// Both edges use `Samples`'s named subtraction verb `remaining_after`
    /// rather than `-`: `Samples` deliberately has no `Sub`, because an unsigned
    /// count that wraps is a length that reads off the end of the world.
    pub fn window(&self, block_len: Samples) -> Range<usize> {
        let start = self.latency.remaining_after(self.block_start).get();
        if start >= block_len.get() {
            return 0..0;
        }
        let remaining = self.output_length.remaining_after(self.kept_so_far).get();
        if remaining == 0 {
            return 0..0;
        }
        let end = block_len.get().min(start + remaining);
        start..end
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor(start: usize, latency: usize, kept: usize, output_length: usize) -> BlockCursor {
        BlockCursor {
            block_start: Samples(start),
            latency: Samples(latency),
            kept_so_far: Samples(kept),
            output_length: Samples(output_length),
        }
    }

    #[test]
    fn a_block_entirely_before_the_gate_is_dropped() {
        assert_eq!(cursor(0, 100, 0, 1000).window(Samples(64)), 0..0);
    }

    #[test]
    fn a_block_straddling_the_gate_keeps_its_tail() {
        // block 80..144, gate at 100 → keep indices 20..64
        assert_eq!(cursor(80, 100, 0, 1000).window(Samples(64)), 20..64);
    }

    #[test]
    fn a_block_past_the_gate_is_kept_whole() {
        assert_eq!(cursor(200, 100, 100, 1000).window(Samples(64)), 0..64);
    }

    #[test]
    fn the_last_block_is_capped_at_the_output_length() {
        // 990 of 1000 kept → at most 10 more
        assert_eq!(cursor(200, 100, 990, 1000).window(Samples(64)), 0..10);
    }

    #[test]
    fn a_block_past_the_cap_is_dropped() {
        assert_eq!(cursor(200, 100, 1000, 1000).window(Samples(64)), 0..0);
    }
}
