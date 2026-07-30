use tutti_core::Arc;
use tutti_core::AtomicF32;
use tutti_core::{AudioUnit, BufferMut, BufferRef, Feedback, Mix, SignalFrame};

use super::modulated_delay::{ModulatedDelay, ModulatedDelayConfig};

/// Stereo chorus effect. 2-in, 2-out.
///
/// Thin wrapper over an internal `ModulatedDelay` with chorus-flavor defaults:
/// longer base delay (10 ms) than flanger, quarter-cycle L/R phase offset.
pub struct ChorusNode {
    core: ModulatedDelay,
}

impl Default for ChorusNode {
    fn default() -> Self {
        Self::new()
    }
}

impl ChorusNode {
    pub fn new() -> Self {
        Self {
            core: ModulatedDelay::new(ModulatedDelayConfig::CHORUS, 1.0, 0.005, 0.3, 0.5),
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
            .store(tutti_core::Seconds(secs.into().get().clamp(0.0, 0.04)));
    }
    pub fn set_feedback(&self, fb: impl Into<Feedback>) {
        self.core
            .mix
            .feedback
            .store(Feedback::new_clamped(fb.into().get()));
    }
    pub fn set_mix(&self, mix: impl Into<Mix>) {
        self.core.mix.mix.store(Mix::new_clamped(mix.into().get()));
    }
}

impl AudioUnit for ChorusNode {
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
        if let Some((param, value)) = tutti_core::unit_param::from_setting(&setting) {
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
        crate::node_id::CHORUS_ID
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut out = SignalFrame::new(2);
        // f64 throughout: `route` wants f64, so the old f32 round-trip was
        // pure loss.
        let delay_samples = self.core.base_delay_secs() as f64 * self.core.sample_rate.get();
        out.set(0, input.at(0).delay(delay_samples));
        out.set(1, input.at(1).delay(delay_samples));
        out
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>() + self.core.footprint()
    }
}

impl Clone for ChorusNode {
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
    fn test_chorus_passthrough_dry() {
        let mut chorus = ChorusNode::new();
        chorus.set_sample_rate(tutti_core::SampleRate(44100.0));
        chorus.set_mix(0.0);

        let mut out = [0.0f32; 2];
        chorus.tick(&[0.5, -0.3], &mut out);
        assert!((out[0] - 0.5).abs() < 0.001);
        assert!((out[1] - (-0.3)).abs() < 0.001);
    }

    #[test]
    fn test_chorus_produces_output() {
        let mut chorus = ChorusNode::new();
        chorus.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut out = [0.0f32; 2];
        for _ in 0..1000 {
            chorus.tick(&[1.0, 1.0], &mut out);
        }
        assert!(
            out[0].abs() > 0.01,
            "Chorus should produce output: {}",
            out[0]
        );
        assert!(
            out[1].abs() > 0.01,
            "Chorus should produce output: {}",
            out[1]
        );
    }

    #[test]
    fn test_chorus_stereo_difference() {
        let mut chorus = ChorusNode::new();
        chorus.set_sample_rate(tutti_core::SampleRate(44100.0));
        chorus.set_mix(1.0);

        let mut out = [0.0f32; 2];
        let mut l_sum = 0.0f64;
        let mut r_sum = 0.0f64;
        for _ in 0..4410 {
            chorus.tick(&[1.0, 1.0], &mut out);
            l_sum += out[0] as f64;
            r_sum += out[1] as f64;
        }
        assert!(
            (l_sum - r_sum).abs() > 0.01,
            "Stereo channels should differ"
        );
    }

    #[test]
    fn test_chorus_reset() {
        let mut chorus = ChorusNode::new();
        chorus.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut out = [0.0f32; 2];
        for _ in 0..100 {
            chorus.tick(&[1.0, 1.0], &mut out);
        }
        chorus.reset();
        chorus.tick(&[0.0, 0.0], &mut out);
        assert!(
            out[0].abs() < 0.01,
            "After reset, output should be near zero"
        );
    }
}
