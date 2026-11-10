//! Disk read throughput, used to size refill chunks.
//!
//! Deliberately just the one measurement. A counter nothing loads is not
//! instrumentation, it is cost on every read — so the only figure kept here is
//! the one varifill actually consults.
//!
//! `read_rate` is consulted unconditionally, but `record_read` — the only thing
//! that feeds it — is called from the codec-gated decode arm of
//! `decode::load_wave`. With no codec feature on, the meter is read and
//! never written, so its internals read as dead. Scoped to that build rather
//! than allowed outright.
//! (No `test` arm on the gate below, unlike `cache.rs`: this module has no
//! tests of its own, so a codec-free `--all-targets` build still finds no
//! caller.)
#![cfg_attr(
    not(codec),
    allow(
        dead_code,
        reason = "read-only without a codec feature: `record_read`'s sole caller is the codec-gated decode arm"
    )
)]

use atomic_float::AtomicF64;
use std::sync::atomic::Ordering;
use std::time::Instant;
use tutti_types::Seconds;

/// Disk read throughput, the sole input varifill takes from measurement rather
/// than from the ring's own fill level.
///
/// The figure is one atomic: the decode thread, the only place a read is
/// timed, publishes it ([`ReadMeter`] computes it) and the streamer thread
/// reads it to size a refill. There is no lock, so a refill never waits for a
/// measurement and no sample is dropped to avoid waiting.
#[derive(Default)]
pub struct Metrics {
    /// Bytes per second over the trailing window; `0.0` before the first read.
    read_rate: AtomicF64,
}

impl Metrics {
    /// A meter reporting a read rate of zero until the decode thread
    /// publishes one.
    pub fn new() -> Self {
        Self::default()
    }

    /// Read throughput in bytes/second over the trailing one-second window, or
    /// `0.0` before the first read. Varifill scales its chunk size by the square
    /// root of this against a 10 MB/s baseline.
    pub fn read_rate(&self) -> f64 {
        self.read_rate.load(Ordering::Relaxed)
    }

    fn publish(&self, rate: f64) {
        self.read_rate.store(rate, Ordering::Relaxed);
    }
}

/// Sliding window throughput tracker (1-second window), owned by the one
/// thread that times reads (the decode thread) and published through
/// [`Metrics`].
pub(super) struct ReadMeter {
    recent_reads: Vec<(u64, Instant)>,
    window: Seconds,
}

impl ReadMeter {
    pub(super) fn new() -> Self {
        Self {
            recent_reads: Vec::with_capacity(64),
            window: Seconds(1.0),
        }
    }

    /// Record that `bytes` were read from disk, timestamped now, and publish
    /// the new rate to `metrics`.
    pub(super) fn record_read(&mut self, bytes: u64, metrics: &Metrics) {
        let now = Instant::now();
        self.recent_reads.push((bytes, now));
        metrics.publish(self.rate(now));
    }

    fn rate(&mut self, now: Instant) -> f64 {
        let cutoff = now - std::time::Duration::from_secs_f64(self.window.get());
        self.recent_reads.retain(|(_, ts)| *ts > cutoff);

        let Some((first, last)) = self.recent_reads.first().zip(self.recent_reads.last()) else {
            return 0.0;
        };

        let total_bytes: u64 = self.recent_reads.iter().map(|(b, _)| *b).sum();
        let duration = last.1.duration_since(first.1).as_secs_f64();
        let denom = if duration > 0.01 {
            duration
        } else {
            self.window.get()
        };
        total_bytes as f64 / denom
    }
}
