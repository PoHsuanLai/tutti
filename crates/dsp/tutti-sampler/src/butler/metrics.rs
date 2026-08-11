//! Butler-thread read throughput, used to size refill chunks.
//!
//! Deliberately just the one measurement. A counter nothing loads is not
//! instrumentation, it is cost on every read — so the only figure kept here is
//! the one varifill actually consults.

use parking_lot::Mutex;
use std::time::Instant;

/// Disk read throughput over a sliding window, the sole input varifill takes
/// from measurement rather than from the ring's own fill level.
///
/// Butler-thread only. `record_read` `try_lock`s and drops the sample on
/// contention: a missed measurement costs a slightly stale chunk size, and
/// blocking a refill to record one would cost an underrun.
pub struct Metrics {
    /// Only accessed from the butler thread.
    throughput: Mutex<ThroughputTracker>,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            throughput: Mutex::new(ThroughputTracker::new()),
        }
    }
}

/// Sliding window throughput tracker (1-second window).
struct ThroughputTracker {
    recent_reads: Vec<(u64, Instant)>,
    window_secs: f64,
    cached_read_rate: f64,
}

impl ThroughputTracker {
    fn new() -> Self {
        Self {
            recent_reads: Vec::with_capacity(64),
            window_secs: 1.0,
            cached_read_rate: 0.0,
        }
    }

    fn record_read(&mut self, bytes: u64) {
        let now = Instant::now();
        self.recent_reads.push((bytes, now));

        self.update_rate(now);
    }

    fn update_rate(&mut self, now: Instant) {
        let cutoff = now - std::time::Duration::from_secs_f64(self.window_secs);
        self.recent_reads.retain(|(_, ts)| *ts > cutoff);

        let Some((first, last)) = self.recent_reads.first().zip(self.recent_reads.last()) else {
            self.cached_read_rate = 0.0;
            return;
        };

        let total_bytes: u64 = self.recent_reads.iter().map(|(b, _)| *b).sum();
        let duration = last.1.duration_since(first.1).as_secs_f64();
        let denom = if duration > 0.01 {
            duration
        } else {
            self.window_secs
        };
        self.cached_read_rate = total_bytes as f64 / denom;
    }

    fn read_rate(&self) -> f64 {
        self.cached_read_rate
    }
}

impl Metrics {
    /// A tracker with an empty window, reporting a read rate of zero until the
    /// first `record_read`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `bytes` were read from disk, timestamped now.
    ///
    /// Dropped silently if the tracker is already locked. Losing a sample only
    /// leaves the rate stale for one refill cycle; waiting on the lock would
    /// stall the refill that is trying to stay ahead of the audio thread.
    #[inline]
    pub fn record_read(&self, bytes: u64) {
        if let Some(mut tracker) = self.throughput.try_lock() {
            tracker.record_read(bytes);
        }
    }

    /// Read throughput in bytes/second over the trailing one-second window, or
    /// `0.0` before the first read. Varifill scales its chunk size by the square
    /// root of this against a 10 MB/s baseline.
    pub fn read_rate(&self) -> f64 {
        self.throughput.lock().read_rate()
    }
}
