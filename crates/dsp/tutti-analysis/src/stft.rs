//! Static Short-Time Fourier Transform (STFT) for spectrogram generation.
//!
//! Computes a full STFT over an audio buffer, producing a 2D magnitude
//! spectrogram suitable for visualization in a spectral editor.

use rustfft::{num_complex::Complex, FftPlanner};

/// Result of a static STFT computation.
#[derive(Debug, Clone)]
pub struct StftResult {
    /// Flat row-major magnitudes: `time_frames * freq_bins` floats.
    /// Each row is one time frame, each column is a frequency bin.
    /// Values are in linear scale, normalized 0.0..1.0 relative to global peak.
    pub magnitudes: Vec<f32>,
    /// Raw unnormalized magnitudes (same layout as `magnitudes`).
    /// Use these for spectral editing + resynthesis.
    pub raw_magnitudes: Vec<f32>,
    /// Phase angle in radians for each bin (same layout as `magnitudes`).
    /// Needed for ISTFT resynthesis.
    pub phases: Vec<f32>,
    /// Number of frequency bins per frame (= window_size / 2).
    pub freq_bins: usize,
    /// Number of time frames.
    pub time_frames: usize,
    /// Sample rate of the input audio.
    pub sample_rate: f64,
    /// FFT window size used.
    pub window_size: usize,
    /// Hop size between frames.
    pub hop_size: usize,
    /// Global maximum magnitude before normalization.
    pub global_max: f32,
}

impl StftResult {
    /// Frequency in Hz for a given bin index.
    pub fn bin_frequency(&self, bin: usize) -> f64 {
        bin as f64 * self.sample_rate / (self.freq_bins as f64 * 2.0)
    }

    /// Get magnitude at a specific (frame, bin) position.
    pub fn at(&self, frame: usize, bin: usize) -> f32 {
        self.magnitudes[frame * self.freq_bins + bin]
    }
}

/// Full complex STFT, including the Nyquist bin.
///
/// Where [`StftResult`] stores magnitude + phase for display (and drops the
/// Nyquist bin), this stores the raw complex bins `0..=N/2` — `N/2 + 1` bins
/// per frame, DC through Nyquist inclusive. This is the edit-fidelity
/// representation: a complex mask of `1+0i` reconstructs the input to float
/// precision through [`istft_complex`](crate::istft::istft_complex) under a
/// COLA-satisfying window pair, which magnitude-only masking cannot.
#[derive(Debug, Clone)]
pub struct ComplexStftResult {
    /// Flat row-major complex bins: `time_frames * bins_per_frame`, where
    /// `bins_per_frame = window_size / 2 + 1` (DC … Nyquist inclusive).
    pub bins: Vec<Complex<f32>>,
    /// Bins per frame = `window_size / 2 + 1`.
    pub bins_per_frame: usize,
    /// Number of time frames.
    pub time_frames: usize,
    /// Sample rate of the input audio.
    pub sample_rate: f64,
    /// FFT window size used.
    pub window_size: usize,
    /// Hop size between frames.
    pub hop_size: usize,
}

impl ComplexStftResult {
    /// Complex bin at `(frame, bin)`.
    pub fn at(&self, frame: usize, bin: usize) -> Complex<f32> {
        self.bins[frame * self.bins_per_frame + bin]
    }

    /// Linear magnitude at `(frame, bin)` — derive the display spectrogram by
    /// pooling these.
    pub fn magnitude(&self, frame: usize, bin: usize) -> f32 {
        self.at(frame, bin).norm()
    }
}

/// Whether `(window_size, hop_size)` satisfy the COLA condition for a Hann
/// analysis × Hann synthesis window pair (product = Hann²).
///
/// Hann² is constant-overlap-add when `hop` divides `window` and the overlap
/// is at least 75% (`window / hop >= 4`). Under this condition the per-sample
/// `window_sum` normalization in [`istft_complex`](crate::istft::istft_complex)
/// is exact, so untouched bins reconstruct to float precision.
pub fn hann_cola_ok(window_size: usize, hop_size: usize) -> bool {
    hop_size > 0 && window_size.is_multiple_of(hop_size) && window_size / hop_size >= 4
}

/// Compute the full complex STFT of a mono buffer (DC … Nyquist inclusive).
///
/// Uses a Hann analysis window. `window_size` / `hop_size` must satisfy
/// [`hann_cola_ok`] so the inverse reconstructs cleanly; this is asserted.
pub fn compute_stft_complex(
    samples: &[f32],
    sample_rate: f64,
    window_size: usize,
    hop_size: usize,
) -> ComplexStftResult {
    assert!(window_size > 0 && hop_size > 0);
    assert!(
        hann_cola_ok(window_size, hop_size),
        "STFT window/hop ({window_size}/{hop_size}) violate Hann² COLA \
         (need window % hop == 0 && window/hop >= 4)"
    );

    let bins_per_frame = window_size / 2 + 1;

    if samples.len() < window_size {
        return ComplexStftResult {
            bins: vec![],
            bins_per_frame,
            time_frames: 0,
            sample_rate,
            window_size,
            hop_size,
        };
    }

    let time_frames = (samples.len() - window_size) / hop_size + 1;

    let hann: Vec<f32> = crate::window::hann(window_size);

    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(window_size);
    let mut scratch = vec![Complex::default(); fft.get_inplace_scratch_len()];
    let mut fft_buf = vec![Complex::default(); window_size];

    let mut bins = Vec::with_capacity(time_frames * bins_per_frame);

    for frame in 0..time_frames {
        let offset = frame * hop_size;
        for (i, val) in fft_buf.iter_mut().enumerate() {
            *val = Complex {
                re: samples[offset + i] * hann[i],
                im: 0.0,
            };
        }
        fft.process_with_scratch(&mut fft_buf, &mut scratch);
        // Keep DC..=Nyquist (N/2 + 1 bins). The rest is conjugate-symmetric.
        bins.extend_from_slice(&fft_buf[..bins_per_frame]);
    }

    ComplexStftResult {
        bins,
        bins_per_frame,
        time_frames,
        sample_rate,
        window_size,
        hop_size,
    }
}

/// Compute the STFT of a mono audio buffer.
///
/// # Arguments
/// - `samples`: Mono audio samples
/// - `sample_rate`: Sample rate in Hz
/// - `window_size`: FFT window size (e.g. 2048). Must be > 0.
/// - `hop_size`: Minimum hop between successive frames (e.g. 512). Must be > 0.
/// - `target_time_frames`: If set, increase hop size to produce approximately
///   this many frames. Avoids computing thousands of FFTs when only a few
///   hundred are needed for display.
///
/// Returns a `StftResult` with magnitude spectrogram.
pub fn compute_stft(
    samples: &[f32],
    sample_rate: f64,
    window_size: usize,
    hop_size: usize,
    target_time_frames: Option<usize>,
) -> StftResult {
    assert!(window_size > 0 && hop_size > 0);

    let freq_bins = window_size / 2;

    if samples.len() < window_size {
        return StftResult {
            magnitudes: vec![],
            raw_magnitudes: vec![],
            phases: vec![],
            freq_bins,
            time_frames: 0,
            sample_rate,
            window_size,
            hop_size,
            global_max: 0.0,
        };
    }

    // If target_time_frames is set, increase hop to produce fewer frames
    let effective_hop = if let Some(target) = target_time_frames {
        (samples.len() - window_size)
            .checked_div(target)
            .map(|max_hop| max_hop.max(hop_size))
            .unwrap_or(hop_size)
    } else {
        hop_size
    };

    let time_frames = (samples.len() - window_size) / effective_hop + 1;

    // Precompute Hann window
    let hann: Vec<f32> = crate::window::hann(window_size);

    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(window_size);
    let mut scratch = vec![Complex::default(); fft.get_inplace_scratch_len()];
    let mut fft_buf = vec![Complex::default(); window_size];

    let mut raw_magnitudes = Vec::with_capacity(time_frames * freq_bins);
    let mut phases = Vec::with_capacity(time_frames * freq_bins);
    let mut global_max: f32 = 0.0;

    for frame in 0..time_frames {
        let offset = frame * effective_hop;

        // Apply Hann window
        for (i, val) in fft_buf.iter_mut().enumerate() {
            *val = Complex {
                re: samples[offset + i] * hann[i],
                im: 0.0,
            };
        }

        fft.process_with_scratch(&mut fft_buf, &mut scratch);

        // Compute magnitudes and phases for positive frequencies
        for c in &fft_buf[..freq_bins] {
            let mag = c.norm();
            let phase = c.im.atan2(c.re);
            raw_magnitudes.push(mag);
            phases.push(phase);
            if mag > global_max {
                global_max = mag;
            }
        }
    }

    // Normalized copy for display
    let mut magnitudes = raw_magnitudes.clone();
    if global_max > 0.0 {
        let inv = 1.0 / global_max;
        for m in &mut magnitudes {
            *m *= inv;
        }
    }

    StftResult {
        magnitudes,
        raw_magnitudes,
        phases,
        freq_bins,
        time_frames,
        sample_rate,
        window_size,
        hop_size: effective_hop,
        global_max,
    }
}

/// Compute STFT for a specific sample range with a given target resolution.
///
/// Enables viewport-driven on-demand computation: only compute the frames
/// visible at the current zoom level.
///
/// # Arguments
/// - `samples`: Full mono audio buffer (range indices reference into this)
/// - `sample_rate`: Sample rate in Hz
/// - `window_size`: FFT window size (e.g. 2048). Must be > 0.
/// - `start_sample`: First sample of the range to analyze
/// - `end_sample`: Last sample (exclusive) of the range
/// - `target_frames`: Desired number of output frames for this range
pub fn compute_stft_range(
    samples: &[f32],
    sample_rate: f64,
    window_size: usize,
    start_sample: usize,
    end_sample: usize,
    target_frames: usize,
) -> StftResult {
    assert!(window_size > 0 && target_frames > 0);

    let freq_bins = window_size / 2;
    let start = start_sample.min(samples.len());
    let end = end_sample.min(samples.len());

    if end <= start || (end - start) < window_size {
        return StftResult {
            magnitudes: vec![],
            raw_magnitudes: vec![],
            phases: vec![],
            freq_bins,
            time_frames: 0,
            sample_rate,
            window_size,
            hop_size: 0,
            global_max: 0.0,
        };
    }

    let range_len = end - start;
    let hop_size = ((range_len - window_size) / target_frames).max(1);
    let time_frames = (range_len - window_size) / hop_size + 1;

    // Precompute Hann window
    let hann: Vec<f32> = crate::window::hann(window_size);

    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(window_size);
    let mut scratch = vec![Complex::default(); fft.get_inplace_scratch_len()];
    let mut fft_buf = vec![Complex::default(); window_size];

    let mut raw_magnitudes = Vec::with_capacity(time_frames * freq_bins);
    let mut phases = Vec::with_capacity(time_frames * freq_bins);
    let mut global_max: f32 = 0.0;

    for frame in 0..time_frames {
        let offset = start + frame * hop_size;

        for (i, val) in fft_buf.iter_mut().enumerate() {
            *val = Complex {
                re: samples[offset + i] * hann[i],
                im: 0.0,
            };
        }

        fft.process_with_scratch(&mut fft_buf, &mut scratch);

        for c in &fft_buf[..freq_bins] {
            let mag = c.norm();
            let phase = c.im.atan2(c.re);
            raw_magnitudes.push(mag);
            phases.push(phase);
            if mag > global_max {
                global_max = mag;
            }
        }
    }

    let mut magnitudes = raw_magnitudes.clone();
    if global_max > 0.0 {
        let inv = 1.0 / global_max;
        for m in &mut magnitudes {
            *m *= inv;
        }
    }

    StftResult {
        magnitudes,
        raw_magnitudes,
        phases,
        freq_bins,
        time_frames,
        sample_rate,
        window_size,
        hop_size,
        global_max,
    }
}

use rustfft::Fft;
use std::sync::Arc;

/// Streaming STFT builder that accumulates samples and computes FFT frames
/// incrementally as they arrive during audio import.
///
/// Usage:
/// 1. Create with `new(sample_rate, total_samples, target_frames, window_size, min_hop)`
/// 2. Call `push_samples()` as new audio arrives — returns count of newly computed frames
/// 3. Call `snapshot()` at any time for the current partial result
/// 4. Call `finish()` when import completes to get the final `StftResult`
pub struct IncrementalStftBuilder {
    samples: Vec<f32>,
    sample_rate: f64,
    #[allow(dead_code)]
    total_samples: usize,
    #[allow(dead_code)]
    target_frames: usize,
    window_size: usize,
    effective_hop: usize,
    freq_bins: usize,
    hann: Vec<f32>,
    fft: Arc<dyn Fft<f32>>,
    scratch: Vec<Complex<f32>>,
    fft_buf: Vec<Complex<f32>>,
    raw_magnitudes: Vec<f32>,
    global_max: f32,
    computed_frames: usize,
    next_frame_offset: usize,
}

impl IncrementalStftBuilder {
    /// Create a new incremental STFT builder.
    ///
    /// # Arguments
    /// - `sample_rate` — Sample rate of the audio
    /// - `total_samples` — Expected total number of samples (from metadata)
    /// - `target_frames` — Desired number of STFT frames for the overview
    /// - `window_size` — FFT window size (e.g. 2048)
    /// - `min_hop` — Minimum hop size (effective hop may be larger for decimation)
    pub fn new(
        sample_rate: f64,
        total_samples: usize,
        target_frames: usize,
        window_size: usize,
        min_hop: usize,
    ) -> Self {
        let freq_bins = window_size / 2;

        // Compute effective hop to produce ~target_frames from total_samples
        let effective_hop = if target_frames > 0 && total_samples > window_size {
            let max_hop = (total_samples - window_size) / target_frames;
            max_hop.max(min_hop)
        } else {
            min_hop
        };

        // Precompute Hann window
        let hann: Vec<f32> = crate::window::hann(window_size);

        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(window_size);
        let scratch = vec![Complex::default(); fft.get_inplace_scratch_len()];
        let fft_buf = vec![Complex::default(); window_size];

        Self {
            samples: Vec::with_capacity(total_samples.min(44100 * 600)), // cap at 10 min
            sample_rate,
            total_samples,
            target_frames,
            window_size,
            effective_hop,
            freq_bins,
            hann,
            fft,
            scratch,
            fft_buf,
            raw_magnitudes: Vec::with_capacity(target_frames * freq_bins),
            global_max: 0.0,
            computed_frames: 0,
            next_frame_offset: 0,
        }
    }

    /// Append new samples and compute any newly possible FFT frames.
    /// Returns the number of new frames computed.
    pub fn push_samples(&mut self, new: &[f32]) -> usize {
        self.samples.extend_from_slice(new);
        let mut new_frames = 0;

        while self.next_frame_offset + self.window_size <= self.samples.len() {
            let offset = self.next_frame_offset;

            // Apply Hann window
            for (i, val) in self.fft_buf.iter_mut().enumerate() {
                *val = Complex {
                    re: self.samples[offset + i] * self.hann[i],
                    im: 0.0,
                };
            }

            self.fft
                .process_with_scratch(&mut self.fft_buf, &mut self.scratch);

            // Extract magnitudes for positive frequencies
            for bin in 0..self.freq_bins {
                let c = self.fft_buf[bin];
                let mag = c.norm();
                self.raw_magnitudes.push(mag);
                if mag > self.global_max {
                    self.global_max = mag;
                }
            }

            self.computed_frames += 1;
            new_frames += 1;
            self.next_frame_offset += self.effective_hop;
        }

        new_frames
    }

    /// Return a snapshot of the current state as normalized magnitudes + metadata.
    /// Returns `(normalized_magnitudes, computed_frames, freq_bins, global_max)`.
    pub fn snapshot(&self) -> (Vec<f32>, usize, usize, f32) {
        let mut magnitudes = self.raw_magnitudes.clone();
        if self.global_max > 0.0 {
            let inv = 1.0 / self.global_max;
            for m in &mut magnitudes {
                *m *= inv;
            }
        }
        (
            magnitudes,
            self.computed_frames,
            self.freq_bins,
            self.global_max,
        )
    }

    /// Finalize into a `StftResult` (no phases for coarse overview).
    pub fn finish(self) -> StftResult {
        let mut magnitudes = self.raw_magnitudes.clone();
        if self.global_max > 0.0 {
            let inv = 1.0 / self.global_max;
            for m in &mut magnitudes {
                *m *= inv;
            }
        }

        StftResult {
            magnitudes,
            raw_magnitudes: self.raw_magnitudes,
            phases: vec![],
            freq_bins: self.freq_bins,
            time_frames: self.computed_frames,
            sample_rate: self.sample_rate,
            window_size: self.window_size,
            hop_size: self.effective_hop,
            global_max: self.global_max,
        }
    }

    /// Access accumulated samples.
    pub fn samples(&self) -> &[f32] {
        &self.samples
    }

    /// Consume and return accumulated samples.
    pub fn into_samples(self) -> Vec<f32> {
        self.samples
    }

    /// Sample rate of the audio being processed.
    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// Effective hop size between frames.
    pub fn effective_hop(&self) -> usize {
        self.effective_hop
    }

    /// Number of frequency bins per frame.
    pub fn freq_bins(&self) -> usize {
        self.freq_bins
    }

    /// Number of frames computed so far.
    pub fn computed_frames(&self) -> usize {
        self.computed_frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stft_sine_wave() {
        let sample_rate = 44100.0;
        let freq = 440.0;
        let duration_secs = 0.1;
        let num_samples = (sample_rate * duration_secs) as usize;

        let samples: Vec<f32> = (0..num_samples)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / sample_rate as f32).sin())
            .collect();

        let result = compute_stft(&samples, sample_rate, 2048, 512, None);

        assert!(result.time_frames > 0);
        assert_eq!(result.freq_bins, 1024);
        assert_eq!(
            result.magnitudes.len(),
            result.time_frames * result.freq_bins
        );

        // The 440Hz bin should have high magnitude
        let bin_440 = (freq as f64 * 2048.0 / sample_rate) as usize;
        let first_frame_mag = result.at(0, bin_440);
        assert!(
            first_frame_mag > 0.5,
            "440Hz bin should be prominent, got {first_frame_mag}"
        );
    }

    #[test]
    fn test_stft_too_short() {
        let result = compute_stft(&[0.0; 100], 44100.0, 2048, 512, None);
        assert_eq!(result.time_frames, 0);
        assert!(result.magnitudes.is_empty());
    }

    #[test]
    fn test_stft_normalization() {
        let samples: Vec<f32> = (0..4096)
            .map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 44100.0).sin())
            .collect();

        let result = compute_stft(&samples, 44100.0, 2048, 512, None);

        let max_mag = result.magnitudes.iter().copied().fold(0.0f32, f32::max);
        assert!(
            (max_mag - 1.0).abs() < 0.01,
            "Peak should be normalized to 1.0, got {max_mag}"
        );
    }

    #[test]
    fn test_stft_decimation() {
        // ~4 minutes of audio at 44.1kHz
        let num_samples = 44100 * 240;
        let samples: Vec<f32> = (0..num_samples)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 44100.0).sin())
            .collect();

        // Without decimation: many frames
        let full = compute_stft(&samples, 44100.0, 2048, 512, None);
        assert!(
            full.time_frames > 20000,
            "Full STFT should have many frames, got {}",
            full.time_frames
        );

        // With decimation: ~300 frames
        let decimated = compute_stft(&samples, 44100.0, 2048, 512, Some(300));
        assert!(
            decimated.time_frames <= 301 && decimated.time_frames >= 299,
            "Decimated STFT should have ~300 frames, got {}",
            decimated.time_frames
        );
        assert_eq!(decimated.freq_bins, full.freq_bins);

        // 440Hz bin should still be prominent in decimated result
        let bin_440 = (440.0 * 2048.0 / 44100.0) as usize;
        let mid_frame = decimated.time_frames / 2;
        let mag = decimated.at(mid_frame, bin_440);
        assert!(
            mag > 0.5,
            "440Hz should be prominent in decimated STFT, got {mag}"
        );
    }

    #[test]
    fn test_stft_range() {
        let sample_rate = 44100.0;
        // 2 seconds of 440Hz sine
        let num_samples = 44100 * 2;
        let samples: Vec<f32> = (0..num_samples)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sample_rate as f32).sin())
            .collect();

        // Compute only the middle 50% of the audio
        let start = num_samples / 4;
        let end = num_samples * 3 / 4;
        let target = 256;

        let result = compute_stft_range(&samples, sample_rate, 2048, start, end, target);

        assert!(result.time_frames > 0, "Should produce frames");
        assert_eq!(result.freq_bins, 1024);
        // Should be close to target
        assert!(
            result.time_frames <= target + 2 && result.time_frames >= target.saturating_sub(2),
            "Should produce ~{target} frames, got {}",
            result.time_frames
        );

        // 440Hz should still be detected in the range
        let bin_440 = (440.0 * 2048.0 / sample_rate) as usize;
        let mid_frame = result.time_frames / 2;
        let mag = result.at(mid_frame, bin_440);
        assert!(
            mag > 0.5,
            "440Hz should be prominent in range STFT, got {mag}"
        );
    }

    #[test]
    fn test_stft_range_too_short() {
        let samples = vec![0.0f32; 1000];
        let result = compute_stft_range(&samples, 44100.0, 2048, 0, 1000, 100);
        assert_eq!(result.time_frames, 0);
    }

    #[test]
    fn test_stft_range_boundary() {
        let samples: Vec<f32> = (0..44100)
            .map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 44100.0).sin())
            .collect();

        // Range extends past buffer end — should clamp
        let result = compute_stft_range(&samples, 44100.0, 2048, 40000, 50000, 10);
        // Should handle boundary gracefully without panicking.
        let _ = result.time_frames;
    }

    // ========================================================================
    // IncrementalStftBuilder tests
    // ========================================================================

    #[test]
    fn test_incremental_stft_push_samples() {
        let sample_rate = 44100.0;
        let num_samples = 44100 * 2; // 2 seconds
        let samples: Vec<f32> = (0..num_samples)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sample_rate as f32).sin())
            .collect();

        let mut builder = IncrementalStftBuilder::new(sample_rate, num_samples, 300, 2048, 512);

        // Feed in chunks
        let chunk_size = 22050; // 0.5s
        let mut total_frames = 0;
        for chunk in samples.chunks(chunk_size) {
            total_frames += builder.push_samples(chunk);
        }

        assert!(total_frames > 0, "Should have computed frames");
        assert_eq!(builder.computed_frames(), total_frames);
        assert_eq!(builder.freq_bins(), 1024);
    }

    #[test]
    fn test_incremental_stft_finish() {
        let sample_rate = 44100.0;
        let num_samples = 44100 * 2;
        let samples: Vec<f32> = (0..num_samples)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sample_rate as f32).sin())
            .collect();

        let mut builder = IncrementalStftBuilder::new(sample_rate, num_samples, 300, 2048, 512);
        builder.push_samples(&samples);

        let result = builder.finish();
        assert!(result.time_frames > 0);
        assert_eq!(result.freq_bins, 1024);
        assert_eq!(
            result.magnitudes.len(),
            result.time_frames * result.freq_bins
        );

        // 440Hz should be prominent
        let bin_440 = (440.0 * 2048.0 / sample_rate) as usize;
        let mid_frame = result.time_frames / 2;
        let mag = result.at(mid_frame, bin_440);
        assert!(mag > 0.5, "440Hz should be prominent, got {mag}");
    }

    #[test]
    fn test_incremental_stft_snapshot() {
        let sample_rate = 44100.0;
        let num_samples = 44100;
        let samples: Vec<f32> = (0..num_samples)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sample_rate as f32).sin())
            .collect();

        let mut builder = IncrementalStftBuilder::new(sample_rate, num_samples, 300, 2048, 512);

        // Feed half
        builder.push_samples(&samples[..num_samples / 2]);
        let (mags1, frames1, bins1, _) = builder.snapshot();
        assert!(frames1 > 0);
        assert_eq!(bins1, 1024);
        assert_eq!(mags1.len(), frames1 * bins1);

        // Feed rest
        builder.push_samples(&samples[num_samples / 2..]);
        let (mags2, frames2, _, _) = builder.snapshot();
        assert!(frames2 >= frames1, "More frames after more samples");
        assert!(mags2.len() >= mags1.len());
    }

    #[test]
    fn test_incremental_matches_static() {
        let sample_rate = 44100.0;
        let num_samples = 44100 * 2;
        let samples: Vec<f32> = (0..num_samples)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sample_rate as f32).sin())
            .collect();

        // Static computation
        let static_result = compute_stft(&samples, sample_rate, 2048, 512, Some(300));

        // Incremental computation
        let mut builder = IncrementalStftBuilder::new(sample_rate, num_samples, 300, 2048, 512);
        builder.push_samples(&samples);
        let incremental_result = builder.finish();

        // Should produce the same number of frames and bins
        assert_eq!(incremental_result.time_frames, static_result.time_frames);
        assert_eq!(incremental_result.freq_bins, static_result.freq_bins);
        assert_eq!(incremental_result.hop_size, static_result.hop_size);

        // Magnitudes should match exactly (same algorithm)
        for (a, b) in incremental_result
            .magnitudes
            .iter()
            .zip(static_result.magnitudes.iter())
        {
            assert!((a - b).abs() < 1e-6, "Magnitudes should match: {a} vs {b}");
        }
    }
}
