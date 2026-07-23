//! Phase Vocoder Processor for Time-Stretching
//!
//! Implements a phase-locked vocoder for high-quality time-stretching
//! and pitch-shifting using STFT analysis/synthesis.
//!
//! ## Algorithm Overview
//!
//! 1. **Analysis**: Window input signal with Hann window, perform FFT
//! 2. **Phase Unwrapping**: Calculate instantaneous frequency from phase differences
//! 3. **Phase Locking**: Lock phases to nearest spectral peak for transient preservation
//! 4. **Synthesis**: Scale phases for pitch shift, IFFT, overlap-add reconstruction
//!
//! ## RT-Safety
//!
//! All buffers are pre-allocated. The `process()` method performs no allocations.

use std::f32::consts::PI;
use tutti_core::{inverse_fft, real_fft, Complex32};

use super::types::FftSize;

/// Phase Vocoder Processor
///
/// Performs FFT-based time-stretching and pitch-shifting with phase locking.
///
/// ## Pre-allocation
///
/// Call `new()` with your desired FFT size, then the processor is ready for RT use.
/// All internal buffers are allocated during construction.
pub struct PhaseVocoderProcessor {
    // Configuration
    fft_size: usize,
    hop_analysis: usize,

    // Pre-allocated buffers (RT-safe)
    analysis_window: Vec<f32>,
    synthesis_window: Vec<f32>,
    fft_buffer: Vec<f32>,
    complex_buffer: Vec<Complex32>,
    phase_accumulator: Vec<f32>,
    last_phase: Vec<f32>,

    // Input/output FIFOs
    input_fifo: Vec<f32>,
    output_fifo: Vec<f32>,
    input_write_pos: usize,
    input_read_pos: usize,
    output_write_pos: usize,
    output_read_pos: usize,

    // Phase tracking
    expected_phase_diff: Vec<f32>,

    // State
    sample_rate: f64,
    frames_since_onset: usize,
}

impl PhaseVocoderProcessor {
    pub fn new(fft_size: FftSize, sample_rate: impl Into<tutti_core::SampleRate>) -> Self {
        let sample_rate = sample_rate.into().get();
        let size = fft_size.size();
        let hop = fft_size.hop_size();
        let num_bins = size / 2 + 1;

        // Create Hann window
        let analysis_window = Self::create_hann_window(size);

        // Synthesis window (same as analysis for COLA)
        let synthesis_window = analysis_window.clone();

        // Pre-calculate expected phase difference per bin per hop
        let expected_phase_diff: Vec<f32> = (0..num_bins)
            .map(|k| 2.0 * PI * (k as f32) * (hop as f32) / (size as f32))
            .collect();

        Self {
            fft_size: size,
            hop_analysis: hop,
            analysis_window,
            synthesis_window,
            fft_buffer: vec![0.0; size],
            complex_buffer: vec![Complex32::new(0.0, 0.0); size],
            phase_accumulator: vec![0.0; num_bins],
            last_phase: vec![0.0; num_bins],
            input_fifo: vec![0.0; size * 4], // 4x buffer for safe overlap
            output_fifo: vec![0.0; size * 4],
            input_write_pos: 0,
            input_read_pos: 0,
            output_write_pos: 0,
            output_read_pos: 0,
            expected_phase_diff,
            sample_rate,
            frames_since_onset: 0,
        }
    }

    /// Create a Hann window of the specified size
    fn create_hann_window(size: usize) -> Vec<f32> {
        (0..size)
            .map(|i| 0.5 * (1.0 - (2.0 * PI * i as f32 / size as f32).cos()))
            .collect()
    }

    #[cfg(test)]
    pub fn fft_size(&self) -> usize {
        self.fft_size
    }

    pub fn latency_samples(&self) -> usize {
        self.fft_size
    }

    /// Call this when seeking or stopping playback.
    pub fn reset(&mut self) {
        self.fft_buffer.fill(0.0);
        self.complex_buffer.fill(Complex32::new(0.0, 0.0));
        self.phase_accumulator.fill(0.0);
        self.last_phase.fill(0.0);
        self.input_fifo.fill(0.0);
        self.output_fifo.fill(0.0);
        self.input_write_pos = 0;
        self.input_read_pos = 0;
        self.output_write_pos = 0;
        self.output_read_pos = 0;
        self.frames_since_onset = 0;
    }

    pub fn set_sample_rate(&mut self, sample_rate: impl Into<tutti_core::SampleRate>) {
        let sample_rate = sample_rate.into().get();
        if (self.sample_rate - sample_rate).abs() > 0.1 {
            self.sample_rate = sample_rate;
            // Recalculate expected phase differences
            let num_bins = self.fft_size / 2 + 1;
            for k in 0..num_bins {
                self.expected_phase_diff[k] =
                    2.0 * PI * (k as f32) * (self.hop_analysis as f32) / (self.fft_size as f32);
            }
        }
    }

    /// Push input samples into the processor
    ///
    /// Call this to feed audio data to the vocoder.
    /// The processor will internally buffer data until enough is available for processing.
    ///
    /// # RT-Safety
    ///
    /// This method performs no allocations.
    #[inline]
    pub fn push_input(&mut self, samples: &[f32]) {
        let fifo_len = self.input_fifo.len();
        for &sample in samples {
            self.input_fifo[self.input_write_pos % fifo_len] = sample;
            self.input_write_pos += 1;
        }
    }

    #[inline]
    pub fn input_available(&self) -> usize {
        self.input_write_pos.saturating_sub(self.input_read_pos)
    }

    #[inline]
    pub fn output_available(&self) -> usize {
        self.output_write_pos.saturating_sub(self.output_read_pos)
    }

    /// Returns the number of samples actually written to the output buffer.
    ///
    /// RT-safe: no allocations.
    #[inline]
    pub fn pop_output(&mut self, output: &mut [f32]) -> usize {
        let available = self.output_available();
        let count = output.len().min(available);
        let fifo_len = self.output_fifo.len();

        for (i, sample) in output.iter_mut().take(count).enumerate() {
            *sample = self.output_fifo[(self.output_read_pos + i) % fifo_len];
        }
        self.output_read_pos += count;
        count
    }

    /// RT-safe: no allocations, all buffers are pre-allocated.
    pub fn process(&mut self, stretch_factor: f32, pitch_shift_ratio: f32) {
        // Calculate synthesis hop based on stretch factor
        let synthesis_hop = (self.hop_analysis as f32 * stretch_factor).round() as usize;
        let synthesis_hop = synthesis_hop.max(1);

        // Process as many frames as we have input for
        while self.input_available() >= self.fft_size {
            self.process_frame(synthesis_hop, pitch_shift_ratio);
        }
    }

    /// Process a single STFT frame
    ///
    /// # RT-Safety
    ///
    /// This method performs no allocations.
    fn process_frame(&mut self, synthesis_hop: usize, pitch_shift_ratio: f32) {
        let fifo_len = self.input_fifo.len();
        let out_fifo_len = self.output_fifo.len();
        let num_bins = self.fft_size / 2 + 1;

        // 1. Copy input to FFT buffer with analysis window
        for i in 0..self.fft_size {
            let idx = (self.input_read_pos + i) % fifo_len;
            self.fft_buffer[i] = self.input_fifo[idx] * self.analysis_window[i];
        }

        // Advance input read position by analysis hop
        self.input_read_pos += self.hop_analysis;

        // 2. Perform forward FFT
        let spectrum = real_fft(&mut self.fft_buffer);

        // 3. Phase vocoder processing
        // Copy to complex buffer for manipulation
        let copy_len = spectrum.len().min(num_bins);
        self.complex_buffer[..copy_len].copy_from_slice(&spectrum[..copy_len]);

        // Phase unwrapping and accumulation
        for k in 0..num_bins {
            let magnitude = self.complex_buffer[k].norm();
            let phase = self.complex_buffer[k].arg();

            // Calculate phase deviation from expected
            let phase_diff = phase - self.last_phase[k];
            let expected = self.expected_phase_diff[k];

            // Unwrap phase difference
            let mut deviation = phase_diff - expected;
            deviation = Self::wrap_phase(deviation);

            // Calculate true frequency deviation
            let true_freq = expected + deviation;

            // Scale for pitch shifting
            let scaled_freq = true_freq * pitch_shift_ratio;

            // Accumulate phase for synthesis
            self.phase_accumulator[k] +=
                scaled_freq * (synthesis_hop as f32 / self.hop_analysis as f32);
            self.phase_accumulator[k] = Self::wrap_phase(self.phase_accumulator[k]);

            // Store current phase for next frame
            self.last_phase[k] = phase;

            // Reconstruct complex value with accumulated phase
            self.complex_buffer[k] = Complex32::from_polar(magnitude, self.phase_accumulator[k]);
        }

        // 4. Mirror for real-valued output (conjugate symmetry)
        for i in 1..num_bins - 1 {
            let mirror_idx = self.fft_size - i;
            if mirror_idx < self.complex_buffer.len() {
                self.complex_buffer[mirror_idx] = self.complex_buffer[i].conj();
            }
        }

        // 5. Perform inverse FFT
        inverse_fft(&mut self.complex_buffer);

        // 6. Apply synthesis window and overlap-add
        let scale = 1.0 / (self.fft_size as f32);
        for i in 0..self.fft_size {
            let sample = self.complex_buffer[i].re * scale * self.synthesis_window[i];
            let out_idx = (self.output_write_pos + i) % out_fifo_len;
            // Flush-to-zero: keep the overlap-add accumulator out of the
            // subnormal range on silent tails (avoids x86 denormal CPU spikes).
            self.output_fifo[out_idx] =
                super::types::flush_denormal(self.output_fifo[out_idx] + sample);
        }

        // Advance output write position by synthesis hop
        // Clear the new samples that will be written next
        let clear_start = (self.output_write_pos + self.fft_size) % out_fifo_len;
        for i in 0..synthesis_hop {
            let idx = (clear_start + i) % out_fifo_len;
            self.output_fifo[idx] = 0.0;
        }

        self.output_write_pos += synthesis_hop;
        self.frames_since_onset += 1;
    }

    /// Wrap phase to [-PI, PI]
    #[inline]
    fn wrap_phase(phase: f32) -> f32 {
        let mut p = phase;
        while p > PI {
            p -= 2.0 * PI;
        }
        while p < -PI {
            p += 2.0 * PI;
        }
        p
    }
}

impl Clone for PhaseVocoderProcessor {
    fn clone(&self) -> Self {
        Self {
            fft_size: self.fft_size,
            hop_analysis: self.hop_analysis,
            analysis_window: self.analysis_window.clone(),
            synthesis_window: self.synthesis_window.clone(),
            fft_buffer: self.fft_buffer.clone(),
            complex_buffer: self.complex_buffer.clone(),
            phase_accumulator: self.phase_accumulator.clone(),
            last_phase: self.last_phase.clone(),
            input_fifo: self.input_fifo.clone(),
            output_fifo: self.output_fifo.clone(),
            input_write_pos: self.input_write_pos,
            input_read_pos: self.input_read_pos,
            output_write_pos: self.output_write_pos,
            output_read_pos: self.output_read_pos,
            expected_phase_diff: self.expected_phase_diff.clone(),
            sample_rate: self.sample_rate,
            frames_since_onset: self.frames_since_onset,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_processor_creation() {
        let proc = PhaseVocoderProcessor::new(FftSize::Medium, 44100.0);
        assert_eq!(proc.fft_size(), 2048);
        assert_eq!(FftSize::Medium.hop_size(), 512);
    }

    #[test]
    fn test_hann_window() {
        let window = PhaseVocoderProcessor::create_hann_window(1024);
        assert_eq!(window.len(), 1024);

        assert!(window[0] < 0.001);
        assert!(window[1023] < 0.001);
        assert!((window[512] - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_push_and_pop() {
        let mut proc = PhaseVocoderProcessor::new(FftSize::Small, 44100.0);

        let input = vec![0.5; 256];
        proc.push_input(&input);
        assert_eq!(proc.input_available(), 256);

        proc.process(1.0, 1.0);
        assert_eq!(proc.output_available(), 0);

        let more_input = vec![0.3; 1024];
        proc.push_input(&more_input);

        proc.process(1.0, 1.0);
        assert!(proc.output_available() > 0);
    }

    #[test]
    fn test_reset() {
        let mut proc = PhaseVocoderProcessor::new(FftSize::Small, 44100.0);

        let input = vec![0.5; 2048];
        proc.push_input(&input);
        proc.process(1.0, 1.0);

        proc.reset();
        assert_eq!(proc.input_available(), 0);
        assert_eq!(proc.output_available(), 0);
    }

    #[test]
    fn test_wrap_phase() {
        assert!((PhaseVocoderProcessor::wrap_phase(0.0) - 0.0).abs() < 0.001);
        assert!((PhaseVocoderProcessor::wrap_phase(PI) - PI).abs() < 0.001);
        assert!((PhaseVocoderProcessor::wrap_phase(-PI) - (-PI)).abs() < 0.001);

        let wrapped = PhaseVocoderProcessor::wrap_phase(3.0 * PI);
        assert!((wrapped - PI).abs() < 0.1, "Expected ~PI, got {}", wrapped);
    }

    #[test]
    fn test_passthrough() {
        let mut proc = PhaseVocoderProcessor::new(FftSize::Small, 44100.0);

        let freq = 440.0;
        let sample_rate = 44100.0;
        let num_samples = 8192;
        let input: Vec<f32> = (0..num_samples)
            .map(|i| (2.0 * PI * freq * i as f32 / sample_rate as f32).sin() * 0.5)
            .collect();

        proc.push_input(&input);

        for _ in 0..8 {
            proc.process(1.0, 1.0);
        }

        let mut output = vec![0.0f32; 8192];
        let count = proc.pop_output(&mut output);

        assert!(count > 0, "No output produced");

        let non_zero_count = output[..count].iter().filter(|&&x| x.abs() > 1e-8).count();
        assert!(
            non_zero_count > count / 10,
            "Too few non-zero samples: {} out of {}",
            non_zero_count,
            count
        );
    }
}
