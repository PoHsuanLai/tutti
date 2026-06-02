use tutti_core::Arc;
use tutti_core::AtomicF32;
use tutti_core::{AudioUnit, BufferMut, BufferRef, SignalFrame};

use super::modulated_delay::{ModulatedDelay, ModulatedDelayConfig};

/// Stereo flanger effect. 2-in, 2-out.
///
/// Thin wrapper over an internal `ModulatedDelay` with flanger-flavor defaults:
/// short base delay (1 ms), shallow LFO depth, high feedback for the
/// signature comb-filter sweep, and half-cycle L/R phase offset.
pub struct FlangerNode {
    core: ModulatedDelay,
}

impl Default for FlangerNode {
    fn default() -> Self {
        Self::new()
    }
}

impl FlangerNode {
    pub fn new() -> Self {
        Self {
            core: ModulatedDelay::new(ModulatedDelayConfig::FLANGER, 0.5, 0.002, 0.7, 0.5),
        }
    }

    pub fn rate(&self) -> Arc<AtomicF32> {
        self.core.lfo.rate.as_atomic()
    }
    pub fn depth(&self) -> Arc<AtomicF32> {
        self.core.mix.depth.as_atomic()
    }
    pub fn feedback(&self) -> Arc<AtomicF32> {
        self.core.mix.feedback.as_atomic()
    }
    pub fn mix(&self) -> Arc<AtomicF32> {
        self.core.mix.mix.as_atomic()
    }

    pub fn set_rate(&self, hz: impl Into<tutti_core::Hz>) {
        self.core
            .lfo
            .rate
            .store(tutti_core::Hz(hz.into().get().max(0.01)));
    }
    pub fn set_depth(&self, secs: impl Into<tutti_core::Seconds>) {
        self.core
            .mix
            .depth
            .store(tutti_core::Seconds(secs.into().get().clamp(0.0001, 0.01)));
    }
    pub fn set_feedback(&self, fb: impl Into<tutti_core::Linear>) {
        self.core
            .mix
            .feedback
            .store(tutti_core::Linear(fb.into().get().clamp(0.0, 0.99)));
    }
    pub fn set_mix(&self, mix: impl Into<tutti_core::Linear>) {
        self.core
            .mix
            .mix
            .store(tutti_core::Linear(mix.into().get().clamp(0.0, 1.0)));
    }
}

impl AudioUnit for FlangerNode {
    fn inputs(&self) -> usize {
        2
    }
    fn outputs(&self) -> usize {
        2
    }

    fn reset(&mut self) {
        self.core.reset();
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.core.set_sample_rate(sample_rate);
    }

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        self.core.process_sample(input[0], input[1], output);
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        let mut out = [0.0f32; 2];
        for i in 0..size {
            self.core
                .process_sample(input.at_f32(0, i), input.at_f32(1, i), &mut out);
            output.set_f32(0, i, out[0]);
            output.set_f32(1, i, out[1]);
        }
    }

    fn set(&mut self, setting: tutti_core::dsp::Setting) {
        if let Some((param, value)) = tutti_core::UnitParam::from_setting(&setting) {
            match param {
                tutti_core::UnitParam::Rate => self.set_rate(value),
                tutti_core::UnitParam::Depth => self.set_depth(value),
                tutti_core::UnitParam::Feedback => self.set_feedback(value),
                tutti_core::UnitParam::Wet => self.set_mix(value),
                _ => {}
            }
        }
    }

    fn get_id(&self) -> u64 {
        tutti_core::node_id::FLANGER_ID
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut out = SignalFrame::new(2);
        let delay_samples = (self.core.base_delay_secs() * self.core.sample_rate as f32) as f64;
        out.set(0, input.at(0).delay(delay_samples));
        out.set(1, input.at(1).delay(delay_samples));
        out
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>() + self.core.footprint()
    }
}

impl Clone for FlangerNode {
    fn clone(&self) -> Self {
        Self {
            core: self.core.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_flanger_passthrough_dry() {
        let mut flanger = FlangerNode::new();
        flanger.set_sample_rate(tutti_core::SampleRate(44100.0));
        flanger.set_mix(0.0);

        let mut out = [0.0f32; 2];
        flanger.tick(&[0.7, -0.4], &mut out);
        assert!((out[0] - 0.7).abs() < 0.001);
        assert!((out[1] - (-0.4)).abs() < 0.001);
    }

    #[test]
    fn test_flanger_produces_output() {
        let mut flanger = FlangerNode::new();
        flanger.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut out = [0.0f32; 2];
        for _ in 0..1000 {
            flanger.tick(&[1.0, 1.0], &mut out);
        }
        assert!(out[0].abs() > 0.01);
        assert!(out[1].abs() > 0.01);
    }

    #[test]
    fn test_flanger_feedback_effect() {
        let mut flanger = FlangerNode::new();
        flanger.set_sample_rate(tutti_core::SampleRate(44100.0));
        flanger.set_feedback(0.9);
        flanger.set_mix(1.0);

        let mut out = [0.0f32; 2];
        flanger.tick(&[1.0, 1.0], &mut out);

        let mut max_output = 0.0f32;
        for _ in 0..500 {
            flanger.tick(&[0.0, 0.0], &mut out);
            max_output = max_output.max(out[0].abs());
        }
        assert!(
            max_output > 0.01,
            "High feedback should sustain signal: {max_output}"
        );
    }

    #[test]
    fn test_flanger_reset() {
        let mut flanger = FlangerNode::new();
        flanger.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut out = [0.0f32; 2];
        for _ in 0..100 {
            flanger.tick(&[1.0, 1.0], &mut out);
        }
        flanger.reset();
        flanger.tick(&[0.0, 0.0], &mut out);
        assert!(out[0].abs() < 0.01);
    }
}
