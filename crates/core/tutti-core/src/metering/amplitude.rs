//! Lock-free amplitude metering.

use crate::{AtomicBool, AtomicF32, Ordering};
use tutti_types::{Amplitude, StereoPlanes};

/// One meter reading: peak and RMS for a stereo pair.
///
/// A struct rather than a `(f32, f32, f32, f32)` for the reason `measure`
/// takes a [`StereoPlanes`] instead of two loose slices — four same-typed
/// positional values are one careless edit away from swapping a peak with an
/// RMS, and no compiler catches it. Naming them does.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MeterReading {
    pub peak_left: Amplitude,
    pub peak_right: Amplitude,
    pub rms_left: Amplitude,
    pub rms_right: Amplitude,
}

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

    #[inline]
    pub fn get(&self) -> MeterReading {
        MeterReading {
            peak_left: Amplitude(self.peak_left.load(Ordering::Acquire)),
            peak_right: Amplitude(self.peak_right.load(Ordering::Acquire)),
            rms_left: Amplitude(self.rms_left.load(Ordering::Acquire)),
            rms_right: Amplitude(self.rms_right.load(Ordering::Acquire)),
        }
    }

    #[inline]
    pub fn set(&self, reading: MeterReading) {
        self.peak_left
            .store(reading.peak_left.get(), Ordering::Release);
        self.peak_right
            .store(reading.peak_right.get(), Ordering::Release);
        self.rms_left
            .store(reading.rms_left.get(), Ordering::Release);
        self.rms_right
            .store(reading.rms_right.get(), Ordering::Release);
    }

    /// Measure peak + RMS over one deinterleaved stereo buffer and publish.
    ///
    /// RT-safe: reads two slices, does four folds, stores four atomics.
    ///
    /// Takes a [`StereoPlanes`] rather than two loose slices because the RMS
    /// divisor is the pair's *shared* frame count. This was
    /// `measure(left, right)` deriving `frames = left.len()` and never checking
    /// `right`, so a short right channel divided its sum of squares by the wrong
    /// count and published a quietly wrong level. The pairing cannot be formed
    /// unless the two agree, so the bug is now unrepresentable rather than
    /// merely unreached.
    ///
    /// The four folds below read the planes directly rather than through an
    /// accessor per sample — they autovectorize, and the newtype's inner-loop
    /// rule says destructure at the top and index raw below.
    #[inline]
    pub fn measure(&self, planes: StereoPlanes<'_>) {
        let frames = planes.frames();
        if frames == 0 {
            return;
        }
        let (left, right) = (planes.left(), planes.right());
        let peak_l = left.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        let peak_r = right.iter().fold(0.0f32, |m, s| m.max(s.abs()));

        let sum_sq_l: f32 = left.iter().map(|&s| s * s).sum();
        let sum_sq_r: f32 = right.iter().map(|&s| s * s).sum();

        self.set(MeterReading {
            peak_left: Amplitude(peak_l),
            peak_right: Amplitude(peak_r),
            rms_left: Amplitude((sum_sq_l / frames as f32).sqrt()),
            rms_right: Amplitude((sum_sq_r / frames as f32).sqrt()),
        });
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

    /// The last measured buffer's reading.
    pub fn get(&self) -> MeterReading {
        self.amplitude.get()
    }

    /// The shared cell, for a caller that wants to write it directly.
    pub fn cell(&self) -> &std::sync::Arc<AtomicAmplitude> {
        &self.amplitude
    }
}
