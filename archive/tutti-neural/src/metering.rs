//! Lock-free neural inference metering.
//!
//! [`Meter`] lives on a single cache line (`#[repr(align(64))]`). The
//! engine thread writes; any other thread reads via [`Meter::snapshot`],
//! which returns a plain [`Metrics`] value-typed snapshot.
//!
//! Internally the meter is grouped by *what gets read together* rather than a
//! flat soup of atomics:
//!
//! - Timing — current / peak / EMA inference duration.
//! - Batch — last batch size + registered model count.
//! - Health — heartbeat-driven liveness; owns its own `Instant` start
//!   so [`is_alive`](Meter::is_alive) takes a single `Duration` argument
//!   instead of two raw `u64`s.
//!
//! Units live in the type ([`Duration`] in the snapshot, atomic
//! [`Micros`] internally) rather than in field-name suffixes.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Microsecond newtype around `u64`. Internal storage type for atomic
/// timing fields; converted to/from [`Duration`] at the boundary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Micros(pub u64);

impl From<Duration> for Micros {
    #[inline]
    fn from(d: Duration) -> Self {
        Self(d.as_micros() as u64)
    }
}

impl From<Micros> for Duration {
    #[inline]
    fn from(m: Micros) -> Self {
        Duration::from_micros(m.0)
    }
}

/// Inference timing — current / peak / exponential moving average.
struct Timing {
    current_us: AtomicU64,
    peak_us: AtomicU64,
    /// EMA stored as bit-pattern of `f32` so we can keep one atomic per field.
    average_us_bits: AtomicU32,
    samples: AtomicU32,
}

impl Timing {
    fn new() -> Self {
        Self {
            current_us: AtomicU64::new(0),
            peak_us: AtomicU64::new(0),
            average_us_bits: AtomicU32::new(0.0f32.to_bits()),
            samples: AtomicU32::new(0),
        }
    }

    fn record(&self, elapsed: Duration) {
        let elapsed_us = Micros::from(elapsed).0;
        self.current_us.store(elapsed_us, Ordering::Release);

        if elapsed_us > self.peak_us.load(Ordering::Acquire) {
            self.peak_us.store(elapsed_us, Ordering::Release);
        }

        // EMA over up to 100 samples — caps alpha at 1/101 ≈ 0.01.
        let count = self.samples.fetch_add(1, Ordering::Relaxed);
        let alpha = 1.0 / (count.min(100) + 1) as f32;
        let avg = f32::from_bits(self.average_us_bits.load(Ordering::Acquire));
        let next = avg * (1.0 - alpha) + elapsed_us as f32 * alpha;
        self.average_us_bits
            .store(next.to_bits(), Ordering::Release);
    }

    fn snapshot(&self) -> TimingSnapshot {
        TimingSnapshot {
            current: Duration::from_micros(self.current_us.load(Ordering::Acquire)),
            peak: Duration::from_micros(self.peak_us.load(Ordering::Acquire)),
            average: {
                let avg_us = f32::from_bits(self.average_us_bits.load(Ordering::Acquire));
                Duration::from_nanos((avg_us * 1_000.0) as u64)
            },
        }
    }

    fn reset(&self) {
        self.current_us.store(0, Ordering::Release);
        self.peak_us.store(0, Ordering::Release);
        self.average_us_bits
            .store(0.0f32.to_bits(), Ordering::Release);
        self.samples.store(0, Ordering::Relaxed);
    }
}

/// Batching state — most recent batch size + how many models are loaded.
struct Batch {
    last_size: AtomicU32,
    model_count: AtomicU32,
}

impl Batch {
    fn new() -> Self {
        Self {
            last_size: AtomicU32::new(0),
            model_count: AtomicU32::new(0),
        }
    }

    fn snapshot(&self) -> BatchSnapshot {
        BatchSnapshot {
            last_size: self.last_size.load(Ordering::Relaxed),
            model_count: self.model_count.load(Ordering::Relaxed),
        }
    }
}

/// Heartbeat-driven liveness. Owns its own clock so [`Meter::is_alive`]
/// takes a single `Duration` argument.
struct Health {
    started: Instant,
    /// `Some(elapsed_us + 1)` packed into a `u64`. `0` is reserved for
    /// "never heartbeated" so a heartbeat at `elapsed == 0` doesn't collide
    /// with the cold-start sentinel.
    last_heartbeat_us_plus_one: AtomicU64,
}

impl Health {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            last_heartbeat_us_plus_one: AtomicU64::new(0),
        }
    }

    fn heartbeat(&self) {
        let elapsed_us = self.started.elapsed().as_micros() as u64;
        self.last_heartbeat_us_plus_one
            .store(elapsed_us + 1, Ordering::Release);
    }

    fn is_alive(&self, timeout: Duration) -> bool {
        let raw = self.last_heartbeat_us_plus_one.load(Ordering::Acquire);
        if raw == 0 {
            return true; // not yet started — treat as alive
        }
        let last = raw - 1;
        let now_us = self.started.elapsed().as_micros() as u64;
        now_us.saturating_sub(last) < timeout.as_micros() as u64
    }
}

/// Lock-free inference metrics. One per [`Engine`](crate::Engine).
#[repr(align(64))]
pub struct Meter {
    timing: Timing,
    batch: Batch,
    health: Health,
}

impl Default for Meter {
    fn default() -> Self {
        Self::new()
    }
}

impl Meter {
    pub fn new() -> Self {
        Self {
            timing: Timing::new(),
            batch: Batch::new(),
            health: Health::new(),
        }
    }

    /// Record one completed forward pass.
    pub fn record_inference(&self, elapsed: Duration) {
        self.timing.record(elapsed);
    }

    /// Record the size of the most recent batch dispatched to the backend.
    pub fn record_batch(&self, size: usize) {
        self.batch.last_size.store(size as u32, Ordering::Relaxed);
    }

    /// Update the count of registered models.
    pub fn set_model_count(&self, n: u32) {
        self.batch.model_count.store(n, Ordering::Relaxed);
    }

    /// Mark the engine thread alive. Called every loop iteration.
    pub fn heartbeat(&self) {
        self.health.heartbeat();
    }

    /// `false` if the engine thread hasn't heartbeated within `timeout`.
    pub fn is_alive(&self, timeout: Duration) -> bool {
        self.health.is_alive(timeout)
    }

    /// Take a snapshot. Cheap; reads each atomic exactly once.
    pub fn snapshot(&self) -> Metrics {
        Metrics {
            inference: self.timing.snapshot(),
            batch: self.batch.snapshot(),
        }
    }

    pub fn reset(&self) {
        self.timing.reset();
        self.batch.last_size.store(0, Ordering::Relaxed);
    }
}

/// Snapshot of a [`Meter`]. `Clone`-able, cheap, thread-safe by value.
#[derive(Debug, Clone, Default)]
pub struct Metrics {
    pub inference: TimingSnapshot,
    pub batch: BatchSnapshot,
}

/// Inference-timing snapshot. All fields are wall-clock durations.
#[derive(Debug, Clone, Default)]
pub struct TimingSnapshot {
    /// Most recent forward pass.
    pub current: Duration,
    /// Exponential moving average over the last ~100 passes.
    pub average: Duration,
    /// Largest forward pass observed since the engine started.
    pub peak: Duration,
}

/// Batch-state snapshot.
#[derive(Debug, Clone, Default)]
pub struct BatchSnapshot {
    /// Size of the most recent batch sent to the backend.
    pub last_size: u32,
    /// Number of registered models.
    pub model_count: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_inference() {
        let meter = Meter::new();
        meter.record_inference(Duration::from_micros(100));
        let snap = meter.snapshot();
        assert_eq!(snap.inference.current, Duration::from_micros(100));
        assert!(snap.inference.average > Duration::ZERO);
    }

    #[test]
    fn test_health_check() {
        let meter = Meter::new();
        let timeout = Duration::from_millis(500);
        // Not started yet — alive (cold start).
        assert!(meter.is_alive(timeout));

        meter.heartbeat();
        assert!(meter.is_alive(timeout));

        // Wait past the short timeout — should drop dead.
        std::thread::sleep(Duration::from_millis(30));
        assert!(meter.is_alive(Duration::from_millis(500)));
        assert!(!meter.is_alive(Duration::from_millis(1)));
    }

    #[test]
    fn test_record_batch() {
        let meter = Meter::new();
        meter.record_batch(7);
        assert_eq!(meter.snapshot().batch.last_size, 7);
    }

    #[test]
    fn test_ema_convergence() {
        let meter = Meter::new();
        let elapsed = Duration::from_micros(100);
        for _ in 0..200 {
            meter.record_inference(elapsed);
        }
        let avg = meter.snapshot().inference.average;
        let avg_us = avg.as_nanos() as f64 / 1000.0;
        assert!(
            (avg_us - 100.0).abs() < 1.0,
            "EMA should converge to 100µs, got {avg_us}µs"
        );
    }

    #[test]
    fn test_micros_roundtrip() {
        let d = Duration::from_micros(12345);
        assert_eq!(Micros::from(d), Micros(12345));
        assert_eq!(Duration::from(Micros(12345)), d);
    }
}
