//! Per-block control reads and the ramps that keep them from stepping.
//!
//! The width-generic nodes read every [`Param`](tutti_core::Param) **once per
//! `process` call** (and once per `tick` call) instead of once per sample — the
//! per-sample load was up to four atomics per channel per sample on the ladder,
//! and none of them could change faster than a block anyway, since the control
//! thread writes between callbacks. What a block-rate read *can* do is step:
//! a cutoff or a delay time that jumps at a block edge clicks. So a value that
//! moved since the last block is ramped **linearly across the block**, from the
//! value the previous block ended on to the new one, arriving exactly on the
//! block's last sample.
//!
//! Ending exactly on the target is what makes `tick` (a block of one) agree
//! with the old per-sample read: a one-sample ramp *is* the new value.

/// Samples between coefficient solves on a parameter that moves inside a block.
///
/// A filter whose cutoff is swept (by a block ramp or an audio-rate param port)
/// needs a `tan` per coefficient solve. Solving every sample was the dominant
/// cost of a modulated SVF; solving every 16 samples and interpolating the
/// coefficients linearly between solves costs a quarter of a 64-frame block's
/// `tan`s. 16 samples is 0.33 ms at 48 kHz — far below the rate any musical
/// sweep moves at, so the interpolation error is a small fraction of the change
/// between two solves. Measured against the per-sample solve when the twins
/// were merged: 3e-5 worst on a 200 Hz → 8 kHz cutoff sweep in 85 ms (4.6e-4
/// at 64 samples; exactly 0 at 1), 3.5e-6 on a 2 Hz phaser. The `*_swept`
/// goldens in `tests/width_generic_golden.rs` pin the result.
pub(crate) const COEFF_INTERVAL: usize = 16;

/// Channels a recursive node renders together, sample-inner.
///
/// A recursive filter is latency-bound on one channel: each sample waits on
/// the one before. Running a few channels' independent chains side by side
/// fills that latency (instruction-level parallelism) while each still reads
/// and writes its own planar slice. Measured on the f64 SVF at width 6, one
/// block: all six as one group 0.49 µs, as 3 + 3 0.60 µs, one channel at a
/// time 1.33 µs.
pub(crate) const LANES: usize = 8;

/// How many of the `remaining` channels the next group takes: the groups are
/// spread evenly rather than filled greedily, so twelve channels run as 6 + 6
/// rather than 8 + 4 (a small trailing group leaves latency unfilled).
#[inline]
pub(crate) fn lane_group(remaining: usize) -> usize {
    let groups = remaining.div_ceil(LANES);
    remaining.div_ceil(groups)
}

/// A linear ramp from `from` to `to` over `n` samples.
///
/// Sample `i` (0-based) takes `from + step * (i + 1)` with
/// `step = (to - from) / n`, so the first sample has already moved one step
/// and the last is **exactly** `to` — not `from + step * n`, which can miss
/// `to` by an ulp and would then read as a change on the next block. The one
/// division is paid at construction, not per sample.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Ramp {
    from: f32,
    step: f32,
    to: f32,
    n: usize,
}

impl Ramp {
    /// A ramp over `n` samples (clamped to at least one).
    #[inline]
    pub(crate) fn new(from: f32, to: f32, n: usize) -> Self {
        let n = n.max(1);
        Self {
            from,
            step: (to - from) / n as f32,
            to,
            n,
        }
    }

    /// Whether the ramp moves at all. A flat ramp lets a caller take its
    /// constant-control fast path.
    #[inline]
    pub(crate) fn is_flat(&self) -> bool {
        self.from == self.to
    }

    /// The value at sample `i` of the block.
    #[inline]
    pub(crate) fn at(&self, i: usize) -> f32 {
        if i + 1 >= self.n {
            self.to
        } else {
            self.from + self.step * (i + 1) as f32
        }
    }
}

/// Split `0..size` into control segments of at most [`COEFF_INTERVAL`] samples.
///
/// Yields `(start, end)` half-open ranges. The coefficient solve for a segment
/// happens at its last sample, `end - 1`, and the samples before it interpolate
/// from the previous segment's solve.
///
/// `solve_first` makes sample 0 a segment of its own. A node with nothing to
/// interpolate *from* — its first block, or the first after a rate change —
/// solves there exactly instead of ramping in from a stand-in, which would
/// otherwise lag the whole first segment by a sample.
#[inline]
pub(crate) fn segments(size: usize, solve_first: bool) -> impl Iterator<Item = (usize, usize)> {
    let first = usize::from(solve_first && size > 0);
    core::iter::once((0, 1)).take(first).chain(
        (first..size)
            .step_by(COEFF_INTERVAL)
            .map(move |start| (start, (start + COEFF_INTERVAL).min(size))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The last sample is the target bit-for-bit, whatever the arithmetic.
    ///
    /// Mutation: computing the last sample as `from + step * n` like the rest
    /// fails this for `0.7 -> 0.2`, where that sum lands on `0.19999999` in
    /// f32.
    #[test]
    fn a_ramp_ends_exactly_on_its_target() {
        for (from, to, n) in [(0.7f32, 0.2f32, 64usize), (3.3, 1.1, 17), (5.0, 5.0, 3)] {
            let r = Ramp::new(from, to, n);
            assert_eq!(
                r.at(n - 1).to_bits(),
                to.to_bits(),
                "{from} -> {to} over {n}"
            );
        }
    }

    /// A one-sample ramp is the new value — which is what keeps `tick` equal
    /// to the per-sample read it replaces.
    #[test]
    fn a_one_sample_ramp_is_the_new_value() {
        assert_eq!(Ramp::new(3.0, 9.0, 1).at(0), 9.0);
    }

    /// The ramp starts one step in, not on `from`: `from` was already the
    /// previous block's last sample.
    ///
    /// Mutation: `step * i` instead of `step * (i + 1)` makes the first sample
    /// equal `from` and fails.
    #[test]
    fn the_first_sample_has_already_moved_one_step() {
        let r = Ramp::new(0.0, 1.0, 4);
        assert_eq!(r.at(0), 0.25);
        assert_eq!(r.at(1), 0.5);
    }

    /// Segments tile the block with no gap and no overlap.
    #[test]
    fn segments_tile_the_block() {
        let s: Vec<_> = segments(40, false).collect();
        assert_eq!(s, vec![(0, 16), (16, 32), (32, 40)]);
        assert_eq!(segments(1, false).collect::<Vec<_>>(), vec![(0, 1)]);
        assert_eq!(segments(0, false).count(), 0);
    }

    /// An unprimed block solves its first sample alone, then tiles the rest.
    ///
    /// Mutation: dropping the `take(first)` stage (never isolating sample 0)
    /// fails the first assertion.
    #[test]
    fn solve_first_isolates_sample_zero() {
        let s: Vec<_> = segments(40, true).collect();
        assert_eq!(s, vec![(0, 1), (1, 17), (17, 33), (33, 40)]);
        assert_eq!(segments(1, true).collect::<Vec<_>>(), vec![(0, 1)]);
        assert_eq!(segments(0, true).count(), 0);
    }
}
