//! Lock-free amplitude metering.

use crate::{AtomicF32, Ordering};

/// Lock-free amplitude storage (Peak L/R, RMS L/R).
#[repr(align(64))]
pub struct AtomicAmplitude {
    peak_left: AtomicF32,
    peak_right: AtomicF32,
    rms_left: AtomicF32,
    rms_right: AtomicF32,
}

impl Default for AtomicAmplitude {
    fn default() -> Self {
        Self::new()
    }
}

impl AtomicAmplitude {
    pub fn new() -> Self {
        Self {
            peak_left: AtomicF32::new(0.0),
            peak_right: AtomicF32::new(0.0),
            rms_left: AtomicF32::new(0.0),
            rms_right: AtomicF32::new(0.0),
        }
    }

    /// Returns (peak_l, peak_r, rms_l, rms_r).
    #[inline]
    pub fn get(&self) -> (f32, f32, f32, f32) {
        (
            self.peak_left.load(Ordering::Acquire),
            self.peak_right.load(Ordering::Acquire),
            self.rms_left.load(Ordering::Acquire),
            self.rms_right.load(Ordering::Acquire),
        )
    }

    #[inline]
    pub fn set(&self, peak_l: f32, peak_r: f32, rms_l: f32, rms_r: f32) {
        self.peak_left.store(peak_l, Ordering::Release);
        self.peak_right.store(peak_r, Ordering::Release);
        self.rms_left.store(rms_l, Ordering::Release);
        self.rms_right.store(rms_r, Ordering::Release);
    }
}
