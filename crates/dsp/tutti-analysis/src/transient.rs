//! Onset/transient detection using spectral flux with adaptive thresholding.

use rustfft::{num_complex::Complex, FftPlanner};

const DEFAULT_FFT_SIZE: usize = 1024;
const DEFAULT_HOP_SIZE: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Transient {
    pub sample_position: usize,
    /// Seconds
    pub time: f64,
    /// 0.0..1.0
    pub strength: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum DetectionMethod {
    /// Spectral flux (default, good for most audio)
    #[default]
    SpectralFlux,
    /// High-frequency content (good for percussive material)
    HighFrequencyContent,
    /// Energy-based (simple, fast)
    Energy,
    /// Complex domain (phase-based, very accurate)
    ComplexDomain,
}

pub struct TransientDetector {
    sample_rate: f64,
    fft_size: usize,
    hop_size: usize,
    threshold: f32,
    sensitivity: f32,
    min_gap: usize,
    method: DetectionMethod,
    fft_planner: FftPlanner<f32>,
    window: Vec<f32>,
    prev_magnitudes: Vec<f32>,
    fft_scratch: Vec<Complex<f32>>,
    fft_buffer: Vec<Complex<f32>>,
}

impl TransientDetector {
    pub fn new(sample_rate: impl Into<tutti_core::SampleRate>) -> Self {
        Self::with_params(sample_rate, DEFAULT_FFT_SIZE, DEFAULT_HOP_SIZE)
    }

    pub fn with_params(
        sample_rate: impl Into<tutti_core::SampleRate>,
        fft_size: usize,
        hop_size: usize,
    ) -> Self {
        let sample_rate = sample_rate.into().get();
        let fft_size = fft_size.next_power_of_two();
        let window = Self::create_hann_window(fft_size);

        Self {
            sample_rate,
            fft_size,
            hop_size,
            threshold: 0.3,
            sensitivity: 1.0,
            min_gap: (sample_rate * 0.05) as usize,
            method: DetectionMethod::SpectralFlux,
            fft_planner: FftPlanner::new(),
            window,
            prev_magnitudes: vec![0.0; fft_size / 2],
            fft_scratch: vec![Complex::new(0.0, 0.0); fft_size],
            fft_buffer: vec![Complex::new(0.0, 0.0); fft_size],
        }
    }

    pub fn set_threshold(&mut self, threshold: f32) {
        self.threshold = threshold.clamp(0.0, 1.0);
    }

    pub fn set_sensitivity(&mut self, sensitivity: f32) {
        self.sensitivity = sensitivity.clamp(0.1, 10.0);
    }

    pub fn set_min_gap_ms(&mut self, gap_ms: f32) {
        self.min_gap = (gap_ms / 1000.0 * self.sample_rate as f32) as usize;
    }

    pub fn set_method(&mut self, method: DetectionMethod) {
        self.method = method;
        self.reset();
    }

    pub fn reset(&mut self) {
        self.prev_magnitudes.fill(0.0);
        self.fft_buffer.fill(Complex::new(0.0, 0.0));
    }

    fn create_hann_window(size: usize) -> Vec<f32> {
        (0..size)
            .map(|i| {
                let angle = 2.0 * core::f32::consts::PI * i as f32 / (size - 1) as f32;
                0.5 * (1.0 - angle.cos())
            })
            .collect()
    }

    /// Detect onsets across a whole buffer.
    ///
    /// A function of `samples` alone: the carry from any previous call is
    /// cleared first, so two identical calls return identical results. Without
    /// that, frame 0 diffs against the *previous* call's last frame, which
    /// shifts the adaptive threshold and the strength normalization for every
    /// peak — measured at 39 of 165 windows disagreeing on a real streaming
    /// call pattern, with phantom onsets among them.
    pub fn detect(&mut self, samples: &[f32]) -> Vec<Transient> {
        if samples.len() < self.fft_size {
            return Vec::new();
        }

        self.reset();

        let mut detection_function = Vec::new();
        let num_frames = (samples.len() - self.fft_size) / self.hop_size + 1;

        for frame_idx in 0..num_frames {
            let start = frame_idx * self.hop_size;
            let frame = &samples[start..start + self.fft_size];

            let value = match self.method {
                DetectionMethod::SpectralFlux => self.spectral_flux(frame),
                DetectionMethod::HighFrequencyContent => self.high_frequency_content(frame),
                DetectionMethod::Energy => self.energy(frame),
                DetectionMethod::ComplexDomain => self.complex_domain(frame),
            };

            detection_function.push((start, value));
        }

        let peaks = self.find_peaks(&detection_function);
        let mut transients = Vec::new();
        let mut last_position = 0usize;

        for (position, strength) in peaks {
            if position >= last_position + self.min_gap || last_position == 0 {
                transients.push(Transient {
                    sample_position: position,
                    time: position as f64 / self.sample_rate,
                    strength,
                });
                last_position = position;
            }
        }

        transients
    }

    fn run_fft(&mut self, frame: &[f32]) {
        for (i, (s, w)) in frame.iter().zip(&self.window).enumerate() {
            self.fft_buffer[i] = Complex::new(s * w, 0.0);
        }
        self.fft_buffer[frame.len()..self.fft_size].fill(Complex::new(0.0, 0.0));
        let fft = self.fft_planner.plan_fft_forward(self.fft_size);
        fft.process_with_scratch(&mut self.fft_buffer, &mut self.fft_scratch);
    }

    fn spectral_flux(&mut self, frame: &[f32]) -> f32 {
        self.run_fft(frame);

        let half = self.fft_size / 2;
        let mut flux = 0.0f32;
        for i in 0..half {
            let mag = self.fft_buffer[i].norm();
            let diff = mag - self.prev_magnitudes[i];
            if diff > 0.0 {
                flux += diff;
            }
            self.prev_magnitudes[i] = mag;
        }

        flux * self.sensitivity
    }

    fn high_frequency_content(&mut self, frame: &[f32]) -> f32 {
        self.run_fft(frame);

        let half = self.fft_size / 2;
        let mut hfc = 0.0f32;
        for i in 0..half {
            let weight = (i + 1) as f32;
            hfc += weight * self.fft_buffer[i].norm_sqr();
        }

        hfc.sqrt() * self.sensitivity * 0.01
    }

    fn energy(&self, frame: &[f32]) -> f32 {
        let energy: f32 = frame.iter().map(|s| s * s).sum();
        energy.sqrt() * self.sensitivity
    }

    fn complex_domain(&mut self, frame: &[f32]) -> f32 {
        self.run_fft(frame);

        let half = self.fft_size / 2;
        let mut value = 0.0f32;
        for i in 0..half {
            let mag = self.fft_buffer[i].norm();
            let diff = (mag - self.prev_magnitudes[i]).abs();
            value += diff * diff;
            self.prev_magnitudes[i] = mag;
        }

        value.sqrt() * self.sensitivity
    }

    fn find_peaks(&self, detection_fn: &[(usize, f32)]) -> Vec<(usize, f32)> {
        if detection_fn.is_empty() {
            return Vec::new();
        }

        let mut peaks = Vec::new();

        let len = detection_fn.len() as f32;
        let (sum, sum_sq, max_val) = detection_fn
            .iter()
            .fold((0.0f32, 0.0f32, 0.0f32), |(s, sq, mx), &(_, v)| {
                (s + v, sq + v * v, mx.max(v))
            });
        let mean = sum / len;
        let variance = sum_sq / len - mean * mean;
        let std_dev = variance.sqrt();

        let adaptive_threshold = mean + std_dev * self.threshold * 3.0;

        for i in 1..detection_fn.len() - 1 {
            let (pos, val) = detection_fn[i];
            let (_, prev_val) = detection_fn[i - 1];
            let (_, next_val) = detection_fn[i + 1];

            if val > prev_val && val > next_val && val > adaptive_threshold {
                let strength = if max_val > 0.0 {
                    (val / max_val).min(1.0)
                } else {
                    0.0
                };

                peaks.push((pos, strength));
            }
        }

        peaks
    }

    pub fn cleanup_transients(transients: &mut Vec<Transient>, min_gap_seconds: f64) {
        if transients.len() < 2 {
            return;
        }

        let mut i = 1;
        while i < transients.len() {
            if transients[i].time - transients[i - 1].time < min_gap_seconds {
                // Keep the stronger one
                if transients[i].strength > transients[i - 1].strength {
                    transients.remove(i - 1);
                } else {
                    transients.remove(i);
                }
            } else {
                i += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generate_test_signal(sample_rate: f64, duration: f64, transient_times: &[f64]) -> Vec<f32> {
        let num_samples = (sample_rate * duration) as usize;
        let mut samples = vec![0.0f32; num_samples];

        for &time in transient_times {
            let pos = (time * sample_rate) as usize;
            if pos < num_samples {
                for i in 0..50.min(num_samples - pos) {
                    let decay = (-0.1 * i as f32).exp();
                    samples[pos + i] += decay * 0.8;
                }
            }
        }

        samples
    }

    #[test]
    fn test_detector_creation() {
        let detector = TransientDetector::new(44100.0);
        assert_eq!(detector.fft_size, DEFAULT_FFT_SIZE);
        assert_eq!(detector.hop_size, DEFAULT_HOP_SIZE);
    }

    #[test]
    fn test_detect_simple_transients() {
        let sample_rate = 44100.0;
        let transient_times = vec![0.1, 0.3, 0.5, 0.7];
        let samples = generate_test_signal(sample_rate, 1.0, &transient_times);

        let mut detector = TransientDetector::new(sample_rate);
        detector.set_threshold(0.2);
        detector.set_sensitivity(2.0);

        let detected = detector.detect(&samples);

        assert!(!detected.is_empty(), "Should detect at least one transient");

        for transient in &detected {
            assert!(transient.time >= 0.0 && transient.time <= 1.0);
            assert!(transient.strength >= 0.0 && transient.strength <= 1.0);
        }
    }

    #[test]
    fn test_detection_methods() {
        let sample_rate = 44100.0;
        let samples = generate_test_signal(sample_rate, 0.5, &[0.1, 0.25]);

        for method in [
            DetectionMethod::SpectralFlux,
            DetectionMethod::HighFrequencyContent,
            DetectionMethod::Energy,
            DetectionMethod::ComplexDomain,
        ] {
            let mut detector = TransientDetector::new(sample_rate);
            detector.set_method(method);
            detector.set_threshold(0.2);

            let _detected = detector.detect(&samples);
        }
    }

    /// `detect` must be a function of its argument, for every method.
    ///
    /// The shipped code carried `prev_magnitudes` across calls, so frame 0 of
    /// each call diffed against the *previous* call's last frame. That shifted
    /// the adaptive threshold and the strength normalization, so the second
    /// call on identical input could return different onsets — and on a real
    /// streaming pattern, phantom ones.
    #[test]
    fn detect_is_idempotent_across_calls() {
        let sample_rate = 44100.0;
        let samples = generate_test_signal(sample_rate, 0.5, &[0.1, 0.25]);

        for method in [
            DetectionMethod::SpectralFlux,
            DetectionMethod::HighFrequencyContent,
            DetectionMethod::Energy,
            DetectionMethod::ComplexDomain,
        ] {
            let mut detector = TransientDetector::new(sample_rate);
            detector.set_method(method);
            detector.set_threshold(0.2);

            let first = detector.detect(&samples);
            let second = detector.detect(&samples);

            assert_eq!(
                first.len(),
                second.len(),
                "{method:?}: onset count changed on a repeated call"
            );
            for (a, b) in first.iter().zip(&second) {
                assert_eq!(a.sample_position, b.sample_position, "{method:?}: position");
                assert_eq!(a.strength, b.strength, "{method:?}: strength");
            }
        }
    }

    /// A reused detector must agree with a fresh one on every window.
    ///
    /// This is the exact shape the live path drove: successive `detect` calls
    /// on heavily *overlapping* windows (4096 wide, 512 apart) over continuous
    /// tonal material. The overlap is what makes the leak visible — each call
    /// left `prev_magnitudes` at its last frame, and the next call's frame 0
    /// then diffed against a window overlapping it by 3584 samples, producing
    /// a phantom onset at position 512.
    ///
    /// Measured against the unfixed code: 39 of 165 windows disagreed. Note
    /// that disjoint or near-silent windows do *not* expose this — the earlier
    /// smoke tests missed it for exactly that reason.
    #[test]
    fn reused_detector_matches_a_fresh_one() {
        let sample_rate = 44100.0;
        let n = 44100 * 2;
        let signal: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                let env = 0.3 + 0.7 * (2.0 * core::f32::consts::PI * 0.7 * t).sin().abs();
                env * ((2.0 * core::f32::consts::PI * 220.0 * t).sin() * 0.5
                    + (2.0 * core::f32::consts::PI * 1500.0 * t).sin() * 0.3)
            })
            .collect();

        let (window_len, hop) = (4096usize, 512usize);
        let mut shared = TransientDetector::new(sample_rate);
        let mut differing = 0usize;
        let mut total = 0usize;

        let mut start = 0usize;
        while start + window_len <= n {
            let window = &signal[start..start + window_len];

            let from_shared = shared.detect(window);
            let mut fresh = TransientDetector::new(sample_rate);
            let from_fresh = fresh.detect(window);

            total += 1;
            let same = from_shared.len() == from_fresh.len()
                && from_shared.iter().zip(&from_fresh).all(|(a, b)| {
                    a.sample_position == b.sample_position && a.strength == b.strength
                });
            if !same {
                differing += 1;
            }
            start += hop;
        }

        assert_eq!(
            differing, 0,
            "reused detector disagreed with a fresh one on {differing}/{total} windows"
        );
    }
}
