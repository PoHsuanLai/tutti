//! Lock-free amplitude metering.

use crate::{AtomicBool, AtomicF32, Ordering};

/// Lock-free amplitude storage (Peak L/R, RMS L/R).
///
/// Written by the audio thread, read by the UI. One of these per thing you
/// want a meter on: the master output owns one (see [`MasterMeter`]), and each
/// channel strip owns its own (see `dawai-model`'s `channel::strip`).
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

    /// Measure peak + RMS over one deinterleaved stereo buffer and publish.
    ///
    /// RT-safe: reads two slices, does four folds, stores four atomics.
    #[inline]
    pub fn measure(&self, left: &[f32], right: &[f32]) {
        let frames = left.len();
        if frames == 0 {
            return;
        }
        let peak_l = left.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        let peak_r = right.iter().fold(0.0f32, |m, s| m.max(s.abs()));

        let sum_sq_l: f32 = left.iter().map(|&s| s * s).sum();
        let sum_sq_r: f32 = right.iter().map(|&s| s * s).sum();

        self.set(
            peak_l,
            peak_r,
            (sum_sq_l / frames as f32).sqrt(),
            (sum_sq_r / frames as f32).sqrt(),
        );
    }
}

/// The master output's meter: an [`AtomicAmplitude`] plus the switch that says
/// whether the audio callback should bother filling it.
///
/// The switch matters because measuring means deinterleaving the callback
/// buffer; when nothing is watching a meter, that work is skipped entirely.
/// Cheap to clone — both halves are shared.
#[derive(Clone, Default)]
pub struct MasterMeter {
    amplitude: std::sync::Arc<AtomicAmplitude>,
    enabled: std::sync::Arc<AtomicBool>,
}

impl MasterMeter {
    pub fn new() -> Self {
        Self::default()
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

    /// Returns (peak_l, peak_r, rms_l, rms_r) as of the last measured buffer.
    pub fn get(&self) -> (f32, f32, f32, f32) {
        self.amplitude.get()
    }

    /// The shared cell, for a caller that wants to write it directly.
    pub fn cell(&self) -> &std::sync::Arc<AtomicAmplitude> {
        &self.amplitude
    }
}
