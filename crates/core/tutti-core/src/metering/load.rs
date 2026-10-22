//! Render-deadline telemetry: how long each block took against its budget.

use crate::{AtomicF32, AtomicU32, AtomicU64, Ordering, Seconds};

/// The time constant of [`LoadReading::mean`]'s exponential smoothing: a
/// step in the load is 63% through after this long, whatever the block size.
pub const LOAD_MEAN_TIME_CONSTANT: Seconds = Seconds(0.3);

/// One reading of a [`LoadMeter`].
///
/// A load is render time over the block's real-time budget
/// (`frames / sample_rate`): `0.5` used half the time the device allows, and
/// anything above `1.0` missed the deadline. It is a plain `f32`: no unit in
/// `tutti_types::value::units` is a fraction of a time budget (`Mix` and
/// `Depth` are controls with a `0..=1` range, and a load has no upper bound),
/// and a reading has no algebra to guard.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LoadReading {
    /// The last block's load.
    pub last: f32,
    /// The load smoothed over [`LOAD_MEAN_TIME_CONSTANT`] of audio time,
    /// seeded with the first block's load after a start or a
    /// [`reset`](LoadMeter::reset).
    pub mean: f32,
    /// The largest block load since the previous [`take`](LoadMeter::take).
    pub peak: f32,
    /// Blocks over budget (load above `1.0`) since the start or the last
    /// reset.
    pub over_budget: u64,
    /// Blocks measured since the start or the last reset.
    pub blocks: u64,
}

impl LoadReading {
    /// The spike indicator: the peak over the mean. Near `1.0` the load is
    /// steady; far above it a few blocks cost much more than the rest.
    /// `None` before any load was measured.
    pub fn peak_over_mean(&self) -> Option<f32> {
        (self.mean > 0.0).then(|| self.peak / self.mean)
    }
}

/// The engine's DSP-load meter: written once per device block by the audio
/// thread ([`Engine::process`](crate::Engine::process) and
/// [`process_segment`](crate::Engine::process_segment)), read by any thread.
///
/// Built by the engine; a host gets it with
/// [`Engine::load_meter`](crate::Engine::load_meter) and polls
/// [`take`](Self::take). Always on.
///
/// Every field is its own relaxed atomic, and no reading depends on the
/// order of two of them: a reading may straddle two blocks, which a meter
/// accepts. The peak is a `fetch_max` on the bits of a non-negative `f32`
/// (which order like `u32`), and [`take`](Self::take) swaps it to zero: both
/// are read-modify-writes on one location, so a block's peak racing a take
/// lands in this take or the next and is never lost.
#[derive(Debug, Default)]
pub struct LoadMeter {
    last: AtomicF32,
    /// Written only by the audio thread (a load, then a store).
    mean: AtomicF32,
    /// `f32` bits, so the max is one `fetch_max`.
    peak: AtomicU32,
    over_budget: AtomicU64,
    blocks: AtomicU64,
}

impl LoadMeter {
    /// A meter that has measured nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// The current reading, taking the peak: the next take reports the
    /// largest load since this one. A UI polling at frame rate sees every
    /// spike once.
    pub fn take(&self) -> LoadReading {
        LoadReading {
            last: self.last.load(Ordering::Relaxed),
            mean: self.mean.load(Ordering::Relaxed),
            peak: f32::from_bits(self.peak.swap(0, Ordering::Relaxed)),
            over_budget: self.over_budget.load(Ordering::Relaxed),
            blocks: self.blocks.load(Ordering::Relaxed),
        }
    }

    /// Zero the over-budget and block counts and the peak; the mean is
    /// seeded afresh by the next block. A block measured during the reset
    /// may count on either side of it.
    pub fn reset(&self) {
        self.over_budget.store(0, Ordering::Relaxed);
        self.blocks.store(0, Ordering::Relaxed);
        self.peak.store(0, Ordering::Relaxed);
    }

    /// Record one block that took `elapsed` against a budget of `budget`.
    /// Audio thread, one writer: atomics only, no allocation, no lock. A
    /// block with no budget (no frames) records nothing.
    pub(crate) fn record(&self, elapsed: Seconds, budget: Seconds) {
        if budget.0 <= 0.0 {
            return;
        }
        // `max` also maps a NaN to 0, so the bits below are a non-negative
        // float's and `fetch_max` on them is a float max.
        let load = (elapsed / budget).max(0.0);
        let n = self.blocks.fetch_add(1, Ordering::Relaxed);
        let mean = if n == 0 {
            load
        } else {
            // Exact exponential smoothing over audio time, so the mean's
            // response does not depend on the block size.
            let a = 1.0 - (-(budget / LOAD_MEAN_TIME_CONSTANT)).exp();
            let m = self.mean.load(Ordering::Relaxed);
            m + a * (load - m)
        };
        self.mean.store(mean, Ordering::Relaxed);
        self.last.store(load, Ordering::Relaxed);
        self.peak.fetch_max(load.to_bits(), Ordering::Relaxed);
        if load > 1.0 {
            self.over_budget.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: Seconds = Seconds(64.0 / 48_000.0);

    fn at(load: f32) -> Seconds {
        Seconds(BLOCK.0 * load)
    }

    /// A block over budget counts, one at it does not, and `last` is the
    /// block's own load.
    ///
    /// Mutation (run): `load >= 1.0` for the miss test → the block exactly
    /// at budget counts → fails.
    #[test]
    fn only_a_block_past_its_budget_counts_as_over() {
        let m = LoadMeter::new();
        m.record(at(0.5), BLOCK);
        m.record(at(1.0), BLOCK);
        m.record(at(2.5), BLOCK);
        let r = m.take();
        assert_eq!(r.over_budget, 1);
        assert_eq!(r.blocks, 3);
        assert!((r.last - 2.5).abs() < 1e-4, "{}", r.last);
    }

    /// The peak is the largest load since the last take, and a take resets
    /// it: a spike between two polls shows exactly once.
    ///
    /// Mutations (run): `take` loads the peak instead of swapping it → the
    /// second take still reports 3.0 → fails; `record` stores the load as
    /// the peak instead of `fetch_max` → the first take reports 0.2 →
    /// fails.
    #[test]
    fn a_take_reports_the_peak_since_the_last_and_resets_it() {
        let m = LoadMeter::new();
        m.record(at(0.4), BLOCK);
        m.record(at(3.0), BLOCK);
        m.record(at(0.2), BLOCK);
        assert!((m.take().peak - 3.0).abs() < 1e-4);
        assert_eq!(m.take().peak, 0.0, "taken");
        m.record(at(0.7), BLOCK);
        assert!((m.take().peak - 0.7).abs() < 1e-4);
    }

    /// Under a constant load the mean converges to it at the time constant:
    /// after five time constants of audio, within `e^-5` of the step, for
    /// either block size.
    ///
    /// Mutations (run): drop the first-block seed (start the mean at 0) →
    /// the seeded mean is 0, not 0.2 → fails; smoothing by a fixed per-block
    /// factor `a = 0.001` → the 64-frame mean lags far behind → fails.
    #[test]
    fn the_mean_converges_to_a_constant_load_at_its_time_constant() {
        for frames in [64.0f32, 1024.0] {
            let block = Seconds(frames / 48_000.0);
            let m = LoadMeter::new();
            m.record(Seconds(block.0 * 0.2), block);
            assert!((m.take().mean - 0.2).abs() < 1e-6, "seeded");
            let blocks = (5.0 * LOAD_MEAN_TIME_CONSTANT.0 / block.0).ceil() as usize;
            for _ in 0..blocks {
                m.record(Seconds(block.0 * 0.8), block);
            }
            let mean = m.take().mean;
            let bound = 0.6 * (-5.0f32).exp() + 1e-4;
            assert!((mean - 0.8).abs() <= bound, "{frames}: {mean}");
            // And not already there a time constant in: it is smoothed.
            let m = LoadMeter::new();
            m.record(Seconds(block.0 * 0.2), block);
            let one = (LOAD_MEAN_TIME_CONSTANT.0 / block.0).floor() as usize;
            for _ in 0..one {
                m.record(Seconds(block.0 * 0.8), block);
            }
            let mean = m.take().mean;
            assert!(mean < 0.8 - 0.6 * 0.3, "{frames}: {mean}");
        }
    }

    /// A reset zeroes the counts and the peak, and the next block seeds the
    /// mean. A block with no budget records nothing.
    ///
    /// Mutation (run): `reset` leaves `blocks` → the next block is not a
    /// seed, the mean stays near 0.1 → fails.
    #[test]
    fn a_reset_restarts_the_counts_and_the_mean() {
        let m = LoadMeter::new();
        for _ in 0..10 {
            m.record(at(0.1), BLOCK);
        }
        m.record(at(1.5), BLOCK);
        m.reset();
        let r = m.take();
        assert_eq!((r.over_budget, r.blocks, r.peak), (0, 0, 0.0));
        m.record(Seconds(1.0), Seconds(0.0));
        assert_eq!(m.take().blocks, 0, "no budget, no block");
        m.record(at(0.9), BLOCK);
        let r = m.take();
        assert!((r.mean - 0.9).abs() < 1e-4, "{}", r.mean);
        assert_eq!(r.blocks, 1);
    }
}
