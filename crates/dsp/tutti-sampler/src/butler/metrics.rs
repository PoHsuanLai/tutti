//! Butler-thread read throughput, used to size refill chunks.
//!
//! Deliberately just the one measurement: six byte/op/cache counters used to
//! live here too, incremented on every read and never once loaded.

use parking_lot::Mutex;
use std::time::Instant;

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
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn record_read(&self, bytes: u64) {
        if let Some(mut tracker) = self.throughput.try_lock() {
            tracker.record_read(bytes);
        }
    }

    /// Recent read throughput in bytes/second. Used by varifill to adapt chunk sizes.
    pub fn read_rate(&self) -> f64 {
        self.throughput.lock().read_rate()
    }
}
