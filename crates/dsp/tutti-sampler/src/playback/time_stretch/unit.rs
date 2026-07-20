//! Time-stretching audio unit wrapper.

use std::sync::Arc;
use tutti_core::{AtomicF32, AudioUnit, BufferMut, BufferRef, Cents, Ordering, Ratio, SignalFrame};

use tutti_core::RtScratch;

use super::granular::{GrainSize, GranularProcessor};
use super::phase_vocoder::PhaseVocoderProcessor;
use super::types::{Algorithm, FftSize};

enum Processor {
    PhaseVocoder(PhaseVocoderProcessor),
    Granular(GranularProcessor),
}

impl Processor {
    fn latency_samples(&self) -> usize {
        match self {
            Self::PhaseVocoder(p) => p.latency_samples(),
            Self::Granular(p) => p.latency_samples(),
        }
    }

    fn reset(&mut self) {
        match self {
            Self::PhaseVocoder(p) => p.reset(),
            Self::Granular(p) => p.reset(),
        }
    }

    fn set_sample_rate(&mut self, sr: tutti_core::SampleRate) {
        match self {
            Self::PhaseVocoder(p) => p.set_sample_rate(sr),
            Self::Granular(p) => p.set_sample_rate(sr),
        }
    }

    fn push_input(&mut self, samples: &[f32]) {
        match self {
            Self::PhaseVocoder(p) => p.push_input(samples),
            Self::Granular(p) => p.push_input(samples),
        }
    }

    fn process(&mut self, stretch: f32, pitch_ratio: f32) {
        match self {
            Self::PhaseVocoder(p) => p.process(stretch, pitch_ratio),
            Self::Granular(p) => p.process(stretch, pitch_ratio),
        }
    }

    fn pop_output(&mut self, output: &mut [f32]) -> usize {
        match self {
            Self::PhaseVocoder(p) => p.pop_output(output),
            Self::Granular(p) => p.pop_output(output),
        }
    }
}

impl Clone for Processor {
    fn clone(&self) -> Self {
        match self {
            Self::PhaseVocoder(p) => Self::PhaseVocoder(p.clone()),
            Self::Granular(p) => Self::Granular(p.clone()),
        }
    }
}

/// Maximum buffer size for pre-allocation (covers all common audio interfaces)
const MAX_BUFFER_SIZE: usize = 8192;

/// Real-time time-stretching and pitch-shifting unit.
pub struct Unit {
    source: Box<dyn AudioUnit>,
    processor_left: Processor,
    processor_right: Processor,
    stretch_factor: Arc<AtomicF32>,
    pitch_cents: Arc<AtomicF32>,
    enabled: bool,
    algorithm: Algorithm,
    sample_rate: f64,
    source_buffer: Vec<f32>,
    scratch_left: RtScratch<f32>,
    scratch_right: RtScratch<f32>,
    scratch_out_left: RtScratch<f32>,
    scratch_out_right: RtScratch<f32>,
}

impl Unit {
    /// Create with phase vocoder algorithm (default)
    pub fn new(source: Box<dyn AudioUnit>, sample_rate: impl Into<tutti_core::SampleRate>) -> Self {
        Self::with_fft_size(source, sample_rate, FftSize::default())
    }

    /// Create with custom FFT size (phase vocoder)
    pub fn with_fft_size(
        source: Box<dyn AudioUnit>,
        sample_rate: impl Into<tutti_core::SampleRate>,
        fft_size: FftSize,
    ) -> Self {
        let sample_rate = sample_rate.into().get();
        Self {
            source,
            processor_left: Processor::PhaseVocoder(PhaseVocoderProcessor::new(
                fft_size,
                sample_rate,
            )),
            processor_right: Processor::PhaseVocoder(PhaseVocoderProcessor::new(
                fft_size,
                sample_rate,
            )),
            stretch_factor: Arc::new(AtomicF32::new(1.0)),
            pitch_cents: Arc::new(AtomicF32::new(0.0)),
            enabled: true,
            algorithm: Algorithm::PhaseVocoder,
            sample_rate,
            source_buffer: vec![0.0; 2],
            scratch_left: RtScratch::new(MAX_BUFFER_SIZE),
            scratch_right: RtScratch::new(MAX_BUFFER_SIZE),
            scratch_out_left: RtScratch::new(MAX_BUFFER_SIZE),
            scratch_out_right: RtScratch::new(MAX_BUFFER_SIZE),
        }
    }

    /// Create with granular algorithm (better for drums/transients)
    ///
    /// Note: Granular does NOT support pitch shifting - use phase vocoder for that.
    pub fn with_granular(
        source: Box<dyn AudioUnit>,
        sample_rate: impl Into<tutti_core::SampleRate>,
        grain_size: GrainSize,
    ) -> Self {
        let sample_rate = sample_rate.into().get();
        Self {
            source,
            processor_left: Processor::Granular(GranularProcessor::new(grain_size, sample_rate)),
            processor_right: Processor::Granular(GranularProcessor::new(grain_size, sample_rate)),
            stretch_factor: Arc::new(AtomicF32::new(1.0)),
            pitch_cents: Arc::new(AtomicF32::new(0.0)),
            enabled: true,
            algorithm: Algorithm::Granular,
            sample_rate,
            source_buffer: vec![0.0; 2],
            scratch_left: RtScratch::new(MAX_BUFFER_SIZE),
            scratch_right: RtScratch::new(MAX_BUFFER_SIZE),
            scratch_out_left: RtScratch::new(MAX_BUFFER_SIZE),
            scratch_out_right: RtScratch::new(MAX_BUFFER_SIZE),
        }
    }

    pub fn algorithm(&self) -> Algorithm {
        self.algorithm
    }

    /// Set stretch factor (1.0 = normal, 2.0 = half speed, 0.5 = double speed)
    pub fn set_stretch_factor(&self, factor: Ratio) {
        self.stretch_factor
            .store(factor.get().clamp(0.25, 4.0), Ordering::Release);
    }

    pub fn stretch_factor(&self) -> Ratio {
        Ratio::new(self.stretch_factor.load(Ordering::Acquire))
    }

    /// Get Arc for lock-free external control
    pub fn stretch_factor_arc(&self) -> Arc<AtomicF32> {
        Arc::clone(&self.stretch_factor)
    }

    /// Set pitch shift in cents (only works with PhaseVocoder algorithm)
    pub fn set_pitch_cents(&self, cents: Cents) {
        self.pitch_cents
            .store(cents.get().clamp(-2400.0, 2400.0), Ordering::Release);
    }

    pub fn pitch_cents(&self) -> Cents {
        Cents::new(self.pitch_cents.load(Ordering::Acquire))
    }

    /// Get Arc for lock-free external control
    pub fn pitch_cents_arc(&self) -> Arc<AtomicF32> {
        Arc::clone(&self.pitch_cents)
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn is_processing(&self) -> bool {
        if !self.enabled {
            return false;
        }
        let stretch = self.stretch_factor.load(Ordering::Acquire);
        let pitch = self.pitch_cents.load(Ordering::Acquire);
        (stretch - 1.0).abs() > 0.001 || pitch.abs() > 0.5
    }

    pub fn latency_samples(&self) -> usize {
        self.processor_left.latency_samples()
    }

    pub fn source(&self) -> &dyn AudioUnit {
        &*self.source
    }

    pub fn source_mut(&mut self) -> &mut dyn AudioUnit {
        &mut *self.source
    }

    #[inline]
    fn pitch_ratio(&self) -> f32 {
        2.0_f32.powf(self.pitch_cents.load(Ordering::Acquire) / 1200.0)
    }
}

impl Clone for Unit {
    fn clone(&self) -> Self {
        Self {
            source: self.source.clone(),
            processor_left: self.processor_left.clone(),
            processor_right: self.processor_right.clone(),
            stretch_factor: Arc::new(AtomicF32::new(self.stretch_factor.load(Ordering::Acquire))),
            pitch_cents: Arc::new(AtomicF32::new(self.pitch_cents.load(Ordering::Acquire))),
            enabled: self.enabled,
            algorithm: self.algorithm,
            sample_rate: self.sample_rate,
            source_buffer: self.source_buffer.clone(),
            scratch_left: self.scratch_left.clone(),
            scratch_right: self.scratch_right.clone(),
            scratch_out_left: self.scratch_out_left.clone(),
            scratch_out_right: self.scratch_out_right.clone(),
        }
    }
}

impl AudioUnit for Unit {
    fn inputs(&self) -> usize {
        self.source.inputs()
    }

    fn outputs(&self) -> usize {
        2
    }

    fn reset(&mut self) {
        self.source.reset();
        self.processor_left.reset();
        self.processor_right.reset();
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.sample_rate = sample_rate;
        self.source
            .set_sample_rate(tutti_core::SampleRate(sample_rate));
        self.processor_left
            .set_sample_rate(tutti_core::SampleRate(sample_rate));
        self.processor_right
            .set_sample_rate(tutti_core::SampleRate(sample_rate));
    }

    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        self.source.tick(input, &mut self.source_buffer);

        if !self.is_processing() {
            if output.len() >= 2 {
                output[0] = self.source_buffer[0];
                output[1] = self
                    .source_buffer
                    .get(1)
                    .copied()
                    .unwrap_or(self.source_buffer[0]);
            }
            return;
        }

        let stretch = self.stretch_factor.load(Ordering::Acquire);
        let pitch_ratio = self.pitch_ratio();

        self.processor_left.push_input(&[self.source_buffer[0]]);
        let right = self
            .source_buffer
            .get(1)
            .copied()
            .unwrap_or(self.source_buffer[0]);
        self.processor_right.push_input(&[right]);

        self.processor_left.process(stretch, pitch_ratio);
        self.processor_right.process(stretch, pitch_ratio);

        if output.len() >= 2 {
            let mut left = [0.0f32];
            let mut right = [0.0f32];
            self.processor_left.pop_output(&mut left);
            self.processor_right.pop_output(&mut right);
            output[0] = left[0];
            output[1] = right[0];
        }
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        // `size` past MAX_BUFFER_SIZE is clamped by `RtScratch::active`; the
        // fixed capacity makes a per-block reallocation impossible.
        let has_inputs = self.source.inputs() > 0;
        let mut input_sample = [0.0f32];
        {
            let scratch_left = self.scratch_left.active(size);
            let scratch_right = self.scratch_right.active(size);
            for i in 0..size {
                if has_inputs {
                    input_sample[0] = input.at_f32(0, i);
                    self.source.tick(&input_sample, &mut self.source_buffer);
                } else {
                    self.source.tick(&[], &mut self.source_buffer);
                }
                scratch_left[i] = self.source_buffer[0];
                scratch_right[i] = self
                    .source_buffer
                    .get(1)
                    .copied()
                    .unwrap_or(self.source_buffer[0]);
            }
        }

        if !self.is_processing() {
            let scratch_left = self.scratch_left.active_ref(size);
            let scratch_right = self.scratch_right.active_ref(size);
            for i in 0..size {
                output.set_f32(0, i, scratch_left[i]);
                output.set_f32(1, i, scratch_right[i]);
            }
            return;
        }

        let stretch = self.stretch_factor.load(Ordering::Acquire);
        let pitch_ratio = self.pitch_ratio();

        self.processor_left
            .push_input(self.scratch_left.active_ref(size));
        self.processor_right
            .push_input(self.scratch_right.active_ref(size));

        self.processor_left.process(stretch, pitch_ratio);
        self.processor_right.process(stretch, pitch_ratio);

        let out_left = self.scratch_out_left.active(size);
        let out_right = self.scratch_out_right.active(size);
        out_left.fill(0.0);
        out_right.fill(0.0);

        let left_count = self.processor_left.pop_output(out_left);
        let right_count = self.processor_right.pop_output(out_right);

        for i in 0..size {
            output.set_f32(0, i, if i < left_count { out_left[i] } else { 0.0 });
            output.set_f32(1, i, if i < right_count { out_right[i] } else { 0.0 });
        }
    }

    audio_unit_boilerplate!(id = crate::node_id::TIME_STRETCH_ID);

    fn route(&mut self, input: &SignalFrame, frequency: f64) -> SignalFrame {
        let source = self.source.route(input, frequency);
        let mut out = SignalFrame::new(2);
        let latency = self.processor_left.latency_samples() as f64;
        let left = source.at(0).delay(latency);
        let right = if source.len() > 1 {
            source.at(1).delay(latency)
        } else {
            left
        };
        out.set(0, left);
        out.set(1, right);
        out
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>() + self.source.footprint()
    }

    fn allocate(&mut self) {
        self.source.allocate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct PassthroughUnit;

    impl AudioUnit for PassthroughUnit {
        fn inputs(&self) -> usize {
            0
        }
        fn outputs(&self) -> usize {
            2
        }
        fn reset(&mut self) {}
        fn set_sample_rate(&mut self, _: tutti_core::SampleRate) {}
        fn tick(&mut self, _: &[f32], output: &mut [f32]) {
            if output.len() >= 2 {
                output[0] = 0.5;
                output[1] = 0.5;
            }
        }
        fn process(&mut self, size: usize, _: &BufferRef, output: &mut BufferMut) {
            for i in 0..size {
                output.set_f32(0, i, 0.5);
                output.set_f32(1, i, 0.5);
            }
        }
        fn get_id(&self) -> u64 {
            12345
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
        fn route(&mut self, _: &SignalFrame, _: f64) -> SignalFrame {
            SignalFrame::new(2)
        }
        fn footprint(&self) -> usize {
            0
        }
    }

    impl Clone for PassthroughUnit {
        fn clone(&self) -> Self {
            PassthroughUnit
        }
    }

    #[test]
    fn test_phase_vocoder_creation() {
        let unit = Unit::new(Box::new(PassthroughUnit), 44100.0);
        assert_eq!(unit.algorithm(), Algorithm::PhaseVocoder);
        assert_eq!(unit.outputs(), 2);
    }

    #[test]
    fn test_granular_creation() {
        let unit = Unit::with_granular(Box::new(PassthroughUnit), 44100.0, GrainSize::Medium);
        assert_eq!(unit.algorithm(), Algorithm::Granular);
    }

    #[test]
    fn test_set_parameters() {
        let unit = Unit::new(Box::new(PassthroughUnit), 44100.0);

        unit.set_stretch_factor(Ratio::new(2.0));
        assert!((unit.stretch_factor().get() - 2.0).abs() < 0.001);

        unit.set_pitch_cents(Cents::new(-200.0));
        assert!((unit.pitch_cents().get() - (-200.0)).abs() < 0.001);
    }

    #[test]
    fn test_parameter_clamping() {
        let unit = Unit::new(Box::new(PassthroughUnit), 44100.0);

        unit.set_stretch_factor(Ratio::new(10.0));
        assert!((unit.stretch_factor().get() - 4.0).abs() < 0.001);

        unit.set_stretch_factor(Ratio::new(0.1));
        assert!((unit.stretch_factor().get() - 0.25).abs() < 0.001);
    }

    #[test]
    fn test_passthrough_mode() {
        let mut unit = Unit::new(Box::new(PassthroughUnit), 44100.0);
        assert!(!unit.is_processing());

        let mut output = [0.0f32; 2];
        unit.tick(&[], &mut output);
    }

    #[test]
    fn test_enabled_flag() {
        let mut unit = Unit::new(Box::new(PassthroughUnit), 44100.0);

        unit.set_stretch_factor(Ratio::new(2.0));
        assert!(unit.is_processing());

        unit.set_enabled(false);
        assert!(!unit.is_processing());

        unit.set_enabled(true);
        assert!(unit.is_processing());
    }

    #[test]
    fn test_clone() {
        let unit1 = Unit::new(Box::new(PassthroughUnit), 44100.0);
        unit1.set_stretch_factor(Ratio::new(1.5));

        let unit2 = unit1.clone();
        assert!((unit2.stretch_factor().get() - 1.5).abs() < 0.001);

        unit1.set_stretch_factor(Ratio::new(2.0));
        assert!((unit1.stretch_factor().get() - 2.0).abs() < 0.001);
        assert!((unit2.stretch_factor().get() - 1.5).abs() < 0.001);
    }
}
