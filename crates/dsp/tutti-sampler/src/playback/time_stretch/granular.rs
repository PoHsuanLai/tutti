//! Granular Synthesis Processor for Time-Stretching
//!
//! Implements grain-based time-stretching optimized for transient preservation.
//! Better for drums and percussive material than phase vocoder.
//!
//! ## Algorithm Overview
//!
//! 1. **Grain extraction**: Extract overlapping grains from input
//! 2. **Grain scheduling**: Place grains at stretched positions
//! 3. **Crossfade**: Overlap-add with windowing to smooth transitions
//!
//! ## RT-Safety
//!
//! All buffers are pre-allocated. The `process()` method performs no allocations.

use std::f32::consts::PI;

/// Grain size presets
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GrainSize {
    /// 10ms grains - tighter transients, more artifacts on sustained sounds
    Small,
    /// 25ms grains - balanced (default)
    #[default]
    Medium,
    /// 50ms grains - smoother sustained sounds, smeared transients
    Large,
}

impl GrainSize {
    /// Get grain size in samples at given sample rate
    pub fn samples(&self, sample_rate: f64) -> usize {
        let ms = match self {
            Self::Small => 10.0,
            Self::Medium => 25.0,
            Self::Large => 50.0,
        };
        (sample_rate * ms / 1000.0) as usize
    }
}

/// Granular Synthesis Processor
///
/// Time-stretches audio by extracting and repositioning grains.
/// Does NOT support pitch shifting (use phase vocoder for that).
pub struct GranularProcessor {
    // Configuration
    grain_size: usize,
    hop_size: usize, // overlap = grain_size - hop_size

    // Pre-allocated buffers
    window: Vec<f32>,
    input_fifo: Vec<f32>,
    output_fifo: Vec<f32>,

    // Positions
    input_write_pos: usize,
    input_read_pos: usize,
    output_write_pos: usize,
    output_read_pos: usize,

    // Fractional read position for smooth stretching
    fractional_input_pos: f64,

    sample_rate: f64,
}

impl GranularProcessor {
    /// Create a new granular processor
    pub fn new(grain_size: GrainSize, sample_rate: impl Into<tutti_core::SampleRate>) -> Self {
        let sample_rate = sample_rate.into().get();
        let size = grain_size.samples(sample_rate);
        let hop = size / 2; // 50% overlap

        // Hann window for smooth crossfades
        let window: Vec<f32> = (0..size)
            .map(|i| 0.5 * (1.0 - (2.0 * PI * i as f32 / size as f32).cos()))
            .collect();

        Self {
            grain_size: size,
            hop_size: hop,
            window,
            input_fifo: vec![0.0; size * 8],
            output_fifo: vec![0.0; size * 8],
            input_write_pos: 0,
            input_read_pos: 0,
            output_write_pos: 0,
            output_read_pos: 0,
            fractional_input_pos: 0.0,
            sample_rate,
        }
    }

    #[cfg(test)]
    pub fn grain_size(&self) -> usize {
        self.grain_size
    }

    pub fn latency_samples(&self) -> usize {
        self.grain_size
    }

    pub fn reset(&mut self) {
        self.input_fifo.fill(0.0);
        self.output_fifo.fill(0.0);
        self.input_write_pos = 0;
        self.input_read_pos = 0;
        self.output_write_pos = 0;
        self.output_read_pos = 0;
        self.fractional_input_pos = 0.0;
    }

    pub fn set_sample_rate(&mut self, sample_rate: impl Into<tutti_core::SampleRate>) {
        let sample_rate = sample_rate.into().get();
        if (self.sample_rate - sample_rate).abs() > 0.1 {
            self.sample_rate = sample_rate;
            // Recalculate grain size would require reallocation, skip for RT safety
        }
    }

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

    /// Process with time-stretching
    ///
    /// Note: Granular synthesis doesn't naturally support pitch shifting.
    /// The pitch_shift_ratio parameter is ignored (use phase vocoder for pitch).
    pub fn process(&mut self, stretch_factor: f32, _pitch_shift_ratio: f32) {
        // Output hop = input hop * stretch_factor
        let output_hop = (self.hop_size as f32 * stretch_factor).round() as usize;
        let output_hop = output_hop.max(1);

        // Process grains while we have enough input
        while self.input_available() >= self.grain_size + self.hop_size {
            self.process_grain(output_hop);
        }
    }

    /// Process a single grain
    fn process_grain(&mut self, output_hop: usize) {
        let in_fifo_len = self.input_fifo.len();
        let out_fifo_len = self.output_fifo.len();

        // Extract grain with window
        let read_start = self.input_read_pos;
        for i in 0..self.grain_size {
            let in_idx = (read_start + i) % in_fifo_len;
            let out_idx = (self.output_write_pos + i) % out_fifo_len;

            // Overlap-add: add windowed grain to output. Flush-to-zero keeps
            // the accumulator out of the subnormal range on silent tails
            // (avoids x86 denormal CPU spikes); inaudible for normal signals.
            self.output_fifo[out_idx] = super::types::flush_denormal(
                self.output_fifo[out_idx] + self.input_fifo[in_idx] * self.window[i],
            );
        }

        // Advance input by analysis hop (always same hop for input)
        self.input_read_pos += self.hop_size;

        // Advance output by synthesis hop (stretched)
        // Clear upcoming output region
        let clear_start = (self.output_write_pos + self.grain_size) % out_fifo_len;
        for i in 0..output_hop {
            let idx = (clear_start + i) % out_fifo_len;
            self.output_fifo[idx] = 0.0;
        }

        self.output_write_pos += output_hop;
    }
}

impl Clone for GranularProcessor {
    fn clone(&self) -> Self {
        Self {
            grain_size: self.grain_size,
            hop_size: self.hop_size,
            window: self.window.clone(),
            input_fifo: self.input_fifo.clone(),
            output_fifo: self.output_fifo.clone(),
            input_write_pos: self.input_write_pos,
            input_read_pos: self.input_read_pos,
            output_write_pos: self.output_write_pos,
            output_read_pos: self.output_read_pos,
            fractional_input_pos: self.fractional_input_pos,
            sample_rate: self.sample_rate,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grain_size_samples() {
        let sr = 44100.0;
        assert_eq!(GrainSize::Small.samples(sr), 441); // 10ms
        assert_eq!(GrainSize::Medium.samples(sr), 1102); // 25ms
        assert_eq!(GrainSize::Large.samples(sr), 2205); // 50ms
    }

    #[test]
    fn test_processor_creation() {
        let proc = GranularProcessor::new(GrainSize::Medium, 44100.0);
        assert_eq!(proc.grain_size(), 1102);
    }

    #[test]
    fn test_push_and_process() {
        let mut proc = GranularProcessor::new(GrainSize::Medium, 44100.0);

        let input = vec![0.5; 4096];
        proc.push_input(&input);

        proc.process(1.0, 1.0);

        assert!(proc.output_available() > 0);
    }

    #[test]
    fn test_stretch_produces_more_output() {
        let mut proc1 = GranularProcessor::new(GrainSize::Medium, 44100.0);
        let mut proc2 = GranularProcessor::new(GrainSize::Medium, 44100.0);

        let input = vec![0.5; 8192];
        proc1.push_input(&input);
        proc2.push_input(&input);

        proc1.process(1.0, 1.0);
        let output1 = proc1.output_available();

        proc2.process(2.0, 1.0);
        let output2 = proc2.output_available();

        assert!(output2 > output1, "2x stretch should produce more output");
    }

    #[test]
    fn test_reset() {
        let mut proc = GranularProcessor::new(GrainSize::Medium, 44100.0);

        let input = vec![0.5; 4096];
        proc.push_input(&input);
        proc.process(1.0, 1.0);

        proc.reset();
        assert_eq!(proc.input_available(), 0);
        assert_eq!(proc.output_available(), 0);
    }
}
