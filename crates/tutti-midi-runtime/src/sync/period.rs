//! The period of a jittery periodic signal, by least squares.
//!
//! A sender's Timing Clocks are a periodic signal observed through the
//! receiver's arrival jitter: tick `i` arrives at `t0 + i * period + e_i`, with
//! `e_i` the delay the transport and the OS add. [`PeriodFit`] fits that line
//! over a sliding window of the last [`WINDOW`] ticks and reads the slope.
//!
//! **Why a line fit and not a mean of the intervals.** The mean of `n - 1`
//! intervals is `(t_last - t_first) / (n - 1)`: every middle arrival cancels, so
//! the estimate rests on two arrivals and carries `sqrt(2) * sigma / (n - 1)` of
//! error for arrival jitter of standard deviation `sigma`. The least-squares slope
//! is the minimum-variance unbiased estimate of the period for independent
//! arrival errors, `sigma * sqrt(12 / (n * (n^2 - 1)))`: for `n = 48` that is
//! `0.0104 * sigma`, a third of the interval mean's `0.030 * sigma`, and the gap
//! widens with the window. A one-pole filter on the intervals would need a long
//! memory for the same noise and lags a tempo ramp by it; the window lags a ramp
//! by half of itself, two beats at most.

use tutti_types::Seconds;

/// Ticks the fit looks back over: two quarter notes at 24 per quarter.
pub(crate) const WINDOW: usize = 48;

/// A sliding window of arrival times, and the line through them.
#[derive(Debug, Clone)]
pub(crate) struct PeriodFit {
    /// The arrival the window's times count from; the first since `clear`.
    origin: Option<Seconds>,
    /// Seconds since `origin`, as a ring: the oldest is `next - len`.
    times: [f64; WINDOW],
    next: usize,
    len: usize,
}

impl PeriodFit {
    pub(crate) const fn new() -> Self {
        Self {
            origin: None,
            times: [0.0; WINDOW],
            next: 0,
            len: 0,
        }
    }

    /// Forgets every arrival.
    pub(crate) fn clear(&mut self) {
        *self = Self::new();
    }

    /// Arrivals in the window.
    pub(crate) const fn len(&self) -> usize {
        self.len
    }

    /// Adds the next tick's arrival, dropping the oldest at capacity.
    pub(crate) fn push(&mut self, at: Seconds) {
        let origin = *self.origin.get_or_insert(at);
        self.times[self.next] = (at - origin).get().max(0.0);
        self.next = (self.next + 1) % WINDOW;
        self.len = (self.len + 1).min(WINDOW);
    }

    /// Seconds per tick: the least-squares slope of arrival time against tick
    /// index. `None` with fewer than two arrivals.
    pub(crate) fn period(&self) -> Option<Seconds> {
        if self.len < 2 {
            return None;
        }
        let n = self.len as f64;
        let oldest = (self.next + WINDOW - self.len) % WINDOW;
        let at = |i: usize| self.times[(oldest + i) % WINDOW];
        let mean_t = (0..self.len).map(at).sum::<f64>() / n;
        let mean_i = (n - 1.0) / 2.0;
        let (mut num, mut den) = (0.0, 0.0);
        for i in 0..self.len {
            let di = i as f64 - mean_i;
            num += di * (at(i) - mean_t);
            den += di * di;
        }
        Some(Seconds::new(num / den))
    }
}
