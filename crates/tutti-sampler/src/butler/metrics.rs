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

    #[inline]
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

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            bytes_read: self.bytes_read.load(Ordering::Relaxed),
            bytes_written: self.bytes_written.load(Ordering::Relaxed),
            read_ops: self.read_ops.load(Ordering::Relaxed),
            write_ops: self.write_ops.load(Ordering::Relaxed),
            cache_hits: self.cache_hits.load(Ordering::Relaxed),
            cache_misses: self.cache_misses.load(Ordering::Relaxed),
            read_rate: self.read_rate(),
        }
    }

    /// Resets cumulative counters. The throughput tracker is intentionally
    /// not cleared — it's a sliding-window live signal, not a counter.
    pub fn reset(&self) {
        self.bytes_read.store(0, Ordering::Relaxed);
        self.bytes_written.store(0, Ordering::Relaxed);
        self.read_ops.store(0, Ordering::Relaxed);
        self.write_ops.store(0, Ordering::Relaxed);
        self.cache_hits.store(0, Ordering::Relaxed);
        self.cache_misses.store(0, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub read_ops: u64,
    pub write_ops: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    /// bytes/second
    pub read_rate: f64,
}

impl Snapshot {
    /// 0.0..1.0. Returns 1.0 if no cache operations have occurred.
    pub fn cache_hit_rate(&self) -> f32 {
        let total = self.cache_hits + self.cache_misses;
        if total == 0 {
            1.0
        } else {
            self.cache_hits as f32 / total as f32
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_recording() {
        let metrics = Metrics::new();

        metrics.record_read(1024);
        metrics.record_read(2048);
        metrics.record_write(512);
        metrics.record_cache_hit();
        metrics.record_cache_hit();
        metrics.record_cache_miss();

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.bytes_read, 3072);
        assert_eq!(snapshot.bytes_written, 512);
        assert_eq!(snapshot.read_ops, 2);
        assert_eq!(snapshot.write_ops, 1);
        assert_eq!(snapshot.cache_hits, 2);
        assert_eq!(snapshot.cache_misses, 1);
    }

    #[test]
    fn test_cache_hit_rate() {
        let snapshot = Snapshot {
            cache_hits: 75,
            cache_misses: 25,
            ..Default::default()
        };
        assert!((snapshot.cache_hit_rate() - 0.75).abs() < 0.001);

        let empty = Snapshot::default();
        assert!((empty.cache_hit_rate() - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_metrics_reset() {
        let metrics = Metrics::new();
        metrics.record_read(1024);
        metrics.record_cache_hit();

        let before = metrics.snapshot();
        assert_eq!(before.bytes_read, 1024);
        assert_eq!(before.cache_hits, 1);

        metrics.reset();

        let after = metrics.snapshot();
        assert_eq!(after.bytes_read, 0);
        assert_eq!(after.cache_hits, 0);
    }
}
