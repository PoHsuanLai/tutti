//! CPU usage tracking for audio callbacks.

use crate::{AtomicBool, AtomicF32, AtomicU32, AtomicU64, Ordering};
use core::time::Duration;

/// CPU metrics snapshot.
#[derive(Debug, Clone, Default)]
pub struct CpuMetrics {
    pub average: f32,
    pub peak: f32,
    pub current: f32,
    pub underruns: u64,
    pub buffer_size: usize,
    pub max_time_us: f64,
    pub actual_time_us: f64,
}

/// CPU meter for audio callback performance tracking.
#[repr(align(64))]
pub struct CpuMeter {
    current: AtomicF32,
    peak: AtomicF32,
    average: AtomicF32,
    underruns: AtomicU64,
    samples: AtomicU32,
    sample_rate: f64,
    enabled: AtomicBool,
}

impl CpuMeter {
    pub fn new(sample_rate: impl Into<crate::SampleRate>) -> Self {
        Self {
            current: AtomicF32::new(0.0),
            peak: AtomicF32::new(0.0),
            average: AtomicF32::new(0.0),
            underruns: AtomicU64::new(0),
            samples: AtomicU32::new(0),
            sample_rate: sample_rate.into().get(),
            enabled: AtomicBool::new(false),
        }
    }

    pub fn enable(&self) {
        self.enabled.store(true, Ordering::Release);
    }

    pub fn disable(&self) {
        self.enabled.store(false, Ordering::Release);
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    pub fn record(&self, buffer_size: usize, elapsed: Duration) {
        if !self.is_enabled() {
            return;
        }

        let max_time = buffer_size as f64 / self.sample_rate;
        let load = (elapsed.as_secs_f64() / max_time) as f32;

        self.current.store(load, Ordering::Release);

        if load > self.peak.load(Ordering::Acquire) {
            self.peak.store(load, Ordering::Release);
        }

        // Exponential moving average
        let count = self.samples.fetch_add(1, Ordering::Relaxed);
        let alpha = 1.0 / (count.min(100) + 1) as f32;
        let avg = self.average.load(Ordering::Acquire);
        self.average
            .store(avg * (1.0 - alpha) + load * alpha, Ordering::Release);

        if elapsed.as_secs_f64() > max_time {
            self.underruns.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn metrics(&self, buffer_size: usize) -> CpuMetrics {
        let max_time_us = (buffer_size as f64 / self.sample_rate) * 1_000_000.0;
        let actual_time_us = (self.current.load(Ordering::Acquire) as f64) * max_time_us;

        CpuMetrics {
            average: self.average.load(Ordering::Acquire) * 100.0,
            peak: self.peak.load(Ordering::Acquire) * 100.0,
            current: self.current.load(Ordering::Acquire) * 100.0,
            underruns: self.underruns.load(Ordering::Relaxed),
            buffer_size,
            max_time_us,
            actual_time_us,
        }
    }

    pub fn average_percent(&self) -> f32 {
        self.average.load(Ordering::Acquire) * 100.0
    }

    pub fn peak_percent(&self) -> f32 {
        self.peak.load(Ordering::Acquire) * 100.0
    }

    pub fn current_percent(&self) -> f32 {
        self.current.load(Ordering::Acquire) * 100.0
    }

    pub fn underruns(&self) -> u64 {
        self.underruns.load(Ordering::Relaxed)
    }

    pub fn reset(&self) {
        self.current.store(0.0, Ordering::Release);
        self.peak.store(0.0, Ordering::Release);
        self.average.store(0.0, Ordering::Release);
        self.underruns.store(0, Ordering::Relaxed);
        self.samples.store(0, Ordering::Relaxed);
    }
}
