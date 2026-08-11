//! Pitch Detection using the YIN algorithm
//!
//! Implements monophonic pitch tracking suitable for:
//! - Instrument tuners
//! - Vocal pitch analysis
//! - Auto-tune preprocessing
//! - Melodic transcription
//!
//! ## Algorithm
//!
//! The YIN algorithm (de Cheveigné & Kawahara, 2002) is a robust
//! autocorrelation-based pitch detector. This implementation includes
//! all 6 steps from the original paper:
//!
//! 1. **Difference function** - d(τ) = Σ(x\[j\] - x\[j+τ\])²
//! 2. **Cumulative mean normalized difference** - d'(τ)
//! 3. **Absolute threshold** - Find first τ where d'(τ) < threshold
//! 4. **Parabolic interpolation** - Sub-sample accuracy
//!
//! ## Performance
//!
//! Uses FFT-based autocorrelation via Wiener-Khinchin theorem for O(n log n)
//! computation: r(τ) = IFFT(|FFT(x)|²)

#[derive(Debug, Clone, Copy, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PitchResult {
    /// The detected pitch in [`Hz`]; `Hz(0.0)` if unvoiced.
    ///
    /// Typed to match `PitchDetector`'s own `min_freq`/`max_freq`, so a caller
    /// never re-wraps this crate's output field by field.
    pub frequency: Hz,
    /// Detection strength as a [`Confidence`] reading, `0.0..=1.0`. A
    /// measurement, not a blend — deliberately not `Mix`.
    pub confidence: Confidence,
}

impl PitchResult {
    /// Whether this reading is a real pitch: both a positive frequency and a
    /// non-zero [`Confidence`]. A default (unvoiced) result is neither.
    pub fn is_voiced(&self) -> bool {
        self.frequency > Hz(0.0) && self.confidence > Confidence(0.0)
    }
}

use rustfft::{num_complex::Complex, FftPlanner};
use tutti_types::{Confidence, Hz};

/// YIN pitch detector (de Cheveigné & Kawahara, 2002).
///
/// Uses FFT-based autocorrelation for O(n log n) performance.
pub struct PitchDetector {
    sample_rate: tutti_core::SampleRate,
    min_freq: Hz,
    max_freq: Hz,
    /// The YIN aperiodicity cutoff — a confidence reading, not a blend.
    threshold: Confidence,
    difference: Vec<f32>,
    cumulative_mean: Vec<f32>,
    fft_planner: FftPlanner<f32>,
    fft_buffer: Vec<Complex<f32>>,
    fft_scratch: Vec<Complex<f32>>,
}

impl PitchDetector {
    /// Default range: 50..2000 Hz.
    pub fn with_range(
        sample_rate: impl Into<tutti_core::SampleRate>,
        min_freq: impl Into<Hz>,
        max_freq: impl Into<Hz>,
    ) -> Self {
        let (min_freq, max_freq) = (min_freq.into(), max_freq.into());
        let sample_rate = sample_rate.into();
        let max_period = (sample_rate.get() / f64::from(min_freq.get())) as usize;
        let fft_size = (max_period * 2).next_power_of_two();

        Self {
            sample_rate,
            min_freq,
            max_freq,
            threshold: Confidence(0.1),
            difference: vec![0.0; max_period + 1],
            cumulative_mean: vec![0.0; max_period + 1],
            fft_planner: FftPlanner::new(),
            fft_buffer: vec![Complex::new(0.0, 0.0); fft_size],
            fft_scratch: vec![Complex::new(0.0, 0.0); fft_size],
        }
    }

    /// YIN threshold (0.01..0.5, default 0.1 per the original paper).
    /// Lower = stricter, higher = more permissive.
    pub fn set_threshold(&mut self, threshold: impl Into<Confidence>) {
        self.threshold = Confidence(threshold.into().get().clamp(0.01, 0.5));
    }

    /// Needs at least `buffer_size()` samples.
    pub fn detect(&mut self, samples: &[f32]) -> PitchResult {
        let min_period = (self.sample_rate.get() / f64::from(self.max_freq.get())) as usize;
        let max_period = (self.sample_rate.get() / f64::from(self.min_freq.get())) as usize;
        let max_period = max_period
            .min(samples.len() / 2)
            .min(self.difference.len() - 1);

        if samples.len() < max_period * 2 || max_period <= min_period {
            return PitchResult::default();
        }

        self.compute_difference(samples, max_period);
        self.compute_cumulative_mean(max_period);
        let (period, aperiodicity) = self.find_best_period_full(min_period, max_period);

        if period == 0 {
            return PitchResult::default();
        }

        let refined_period = self.parabolic_interpolation(period, max_period);
        let frequency = Hz((self.sample_rate.get() / refined_period) as f32);
        // Clamped rather than wrapped raw: `Confidence` is a 0..=1 reading, and
        // this is the one place the aperiodicity inversion could leave the range.
        let confidence = Confidence::new_clamped(1.0 - aperiodicity as f32);

        PitchResult {
            frequency,
            confidence,
        }
    }

    /// YIN steps 1-2: `d(τ) = r_x(0,W) + r_x(τ,W) - 2*autocorr(τ)`, with the
    /// autocorrelation taken by FFT via Wiener-Khinchin:
    /// `r(τ) = IFFT(|FFT(x)|²)`.
    ///
    /// `r(τ) = Σ_{j=0}^{W-1} x[j]*x[j+τ]` is the cross-correlation of `x[0..W]`
    /// with `x[0..W+max_period]`, computed in three steps:
    /// 1. FFT of `x[0..W]` zero-padded to `fft_size`
    /// 2. FFT of `x[0..W+max_period]` zero-padded to `fft_size`
    /// 3. `r = IFFT(conj(FFT_a) * FFT_b)`
    fn compute_difference(&mut self, samples: &[f32], max_period: usize) {
        let window = max_period;
        let fft_size = self.fft_buffer.len();
        let usable = samples.len().min(window + max_period);

        // Cumulative squared energy for running window sums
        let mut cum_sq = vec![0.0f64; usable + 1];
        for i in 0..usable {
            cum_sq[i + 1] = cum_sq[i] + (samples[i] as f64) * (samples[i] as f64);
        }

        let energy = |start: usize, len: usize| -> f64 {
            let end = (start + len).min(usable);
            if start < usable {
                cum_sq[end] - cum_sq[start]
            } else {
                0.0
            }
        };

        // FFT of signal a = x[0..W] zero-padded
        let mut fft_a = vec![Complex::new(0.0f32, 0.0); fft_size];
        for i in 0..window.min(usable) {
            fft_a[i] = Complex::new(samples[i], 0.0);
        }

        // FFT of signal b = x[0..W+max_period] zero-padded
        for (i, buf) in self.fft_buffer[..fft_size].iter_mut().enumerate() {
            *buf = if i < usable {
                Complex::new(samples[i], 0.0)
            } else {
                Complex::new(0.0, 0.0)
            };
        }

        let fft_fwd = self.fft_planner.plan_fft_forward(fft_size);
        fft_fwd.process_with_scratch(&mut fft_a, &mut self.fft_scratch);
        fft_fwd.process_with_scratch(&mut self.fft_buffer, &mut self.fft_scratch);

        // Cross-power spectrum: conj(A) * B
        for (a, buf) in fft_a[..fft_size]
            .iter()
            .zip(self.fft_buffer[..fft_size].iter_mut())
        {
            let a_conj = a.conj();
            *buf = a_conj * *buf;
        }

        // Inverse FFT -> cross-correlation
        let fft_inv = self.fft_planner.plan_fft_inverse(fft_size);
        fft_inv.process_with_scratch(&mut self.fft_buffer, &mut self.fft_scratch);

        let inv_n = 1.0 / fft_size as f64;
        self.difference[0] = 0.0;

        for tau in 1..=max_period {
            let autocorr = (self.fft_buffer[tau].re as f64) * inv_n;
            let e0 = energy(0, window);
            let e_tau = energy(tau, window);
            self.difference[tau] = (e0 + e_tau - 2.0 * autocorr).max(0.0) as f32;
        }
    }

    /// YIN step 3: d'(τ) = d(τ) / ((1/τ) * Σ d(j)), d'(0) = 1
    fn compute_cumulative_mean(&mut self, max_period: usize) {
        self.cumulative_mean[0] = 1.0;

        let mut running_sum = 0.0f32;
        for tau in 1..=max_period {
            running_sum += self.difference[tau];
            if running_sum > 1e-10 {
                self.cumulative_mean[tau] = self.difference[tau] * tau as f32 / running_sum;
            } else {
                self.cumulative_mean[tau] = 1.0;
            }
        }
    }

    /// YIN step 4: return the FIRST local minimum below threshold (not the global
    /// minimum) to prevent octave errors from subharmonic detection.
    /// Returns (period, aperiodicity).
    fn find_best_period_full(&self, min_period: usize, max_period: usize) -> (usize, f32) {
        let mut tau = min_period;

        while tau < max_period {
            if self.cumulative_mean[tau] < self.threshold.get() {
                // Walk to the local minimum
                while tau + 1 < max_period
                    && self.cumulative_mean[tau + 1] < self.cumulative_mean[tau]
                {
                    tau += 1;
                }
                return (tau, self.cumulative_mean[tau]);
            }
            tau += 1;
        }

        // Fallback to global minimum for noisy but periodic signals
        let mut best_tau = min_period;
        let mut best_val = self.cumulative_mean[min_period];

        for tau in min_period + 1..=max_period {
            if self.cumulative_mean[tau] < best_val {
                best_val = self.cumulative_mean[tau];
                best_tau = tau;
            }
        }

        if best_val < 0.5 {
            (best_tau, best_val)
        } else {
            (0, 1.0)
        }
    }

    /// YIN step 5: parabolic interpolation for sub-sample accuracy.
    fn parabolic_interpolation(&self, tau: usize, max_period: usize) -> f64 {
        if tau < 1 || tau >= max_period {
            return tau as f64;
        }

        let s0 = self.cumulative_mean[tau - 1] as f64;
        let s1 = self.cumulative_mean[tau] as f64;
        let s2 = self.cumulative_mean[tau + 1] as f64;

        let denominator = 2.0 * (2.0 * s1 - s2 - s0);

        if denominator.abs() > 1e-10 {
            let adjustment = (s2 - s0) / denominator;
            tau as f64 + adjustment
        } else {
            tau as f64
        }
    }
}
