//! I/O statistics and metrics for butler thread.
//!
//! Tracks throughput and cache efficiency.

use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

pub struct Metrics {
    bytes_read: AtomicU64,
    bytes_written: AtomicU64,
    read_ops: AtomicU64,
    write_ops: AtomicU64,
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    /// Only accessed from butler thread
    throughput: Mutex<ThroughputTracker>,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            bytes_read: AtomicU64::new(0),
            bytes_written: AtomicU64::new(0),
            read_ops: AtomicU64::new(0),
            write_ops: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
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
        self.bytes_read.fetch_add(bytes, Ordering::Relaxed);
        self.read_ops.fetch_add(1, Ordering::Relaxed);
        if let Some(mut tracker) = self.throughput.try_lock() {
            tracker.record_read(bytes);
        }
    }

    /// Recent read throughput in bytes/second. Used by varifill to adapt chunk sizes.
    pub fn read_rate(&self) -> f64 {
        self.throughput.lock().read_rate()
    }

    /// Byte-write accounting. Orphaned by the recording teardown (its only
    /// caller was the deleted capture flush); retained for the write-side rebuild.
    #[inline]
    #[allow(dead_code)]
    pub fn record_write(&self, bytes: u64) {
        self.bytes_written.fetch_add(bytes, Ordering::Relaxed);
        self.write_ops.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_cache_hit(&self) {
        self.cache_hits.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_cache_miss(&self) {
        self.cache_misses.fetch_add(1, Ordering::Relaxed);
    }
}

