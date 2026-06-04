#[cfg(not(feature = "std"))]
use tutti_core::compat::Vec;
use tutti_core::Arc;
use tutti_core::AtomicF32;
use tutti_core::{dsp::DEFAULT_SR, AudioUnit, BufferMut, BufferRef, SignalFrame};

use super::shared::{LfoDrive, LinearModMix};
use tutti_core::{Hz, Linear};

const MAX_STAGES: usize = 12;

/// Inclusive frequency range that the phaser LFO sweeps across.
#[derive(Debug, Clone, Copy)]
pub struct FrequencyRange {
    pub min_hz: Hz,
    pub max_hz: Hz,
}

impl FrequencyRange {
    pub fn new(min_hz: impl Into<Hz>, max_hz: impl Into<Hz>) -> Self {
        Self {
            min_hz: min_hz.into(),
            max_hz: max_hz.into(),
        }
    }
}

#[derive(Clone)]
struct AllPassStage {
    x1: f32,
    y1: f32,
}

impl AllPassStage {
    fn new() -> Self {
        Self { x1: 0.0, y1: 0.0 }
    }

    #[inline]
    fn process(&mut self, input: f32, coefficient: f32) -> f32 {
        let y = coefficient * (input - self.y1) + self.x1;
        self.x1 = input;
        self.y1 = y;
        y
    }

    fn reset(&mut self) {
        self.x1 = 0.0;
        self.y1 = 0.0;
    }
}

/// Mono phaser effect. 1-in, 1-out.
/// Chain of all-pass filters modulated by an internal LFO sweeping the
/// configured frequency range.
pub struct PhaserNode {
    stages: Vec<AllPassStage>,
    lfo: LfoDrive,
    mix: LinearModMix,
    feedback_sample: f32,
    sample_rate: f64,
    range: FrequencyRange,
}

impl PhaserNode {
    pub fn new(stages: usize) -> Self {
        let n = stages.clamp(2, MAX_STAGES);
        Self {
            stages: (0..n).map(|_| AllPassStage::new()).collect(),
            // Mono phaser: single LFO, no stereo offset.
            lfo: LfoDrive::new(0.3, 0.0),
            mix: LinearModMix::new(0.5, 0.5, 0.5),
            feedback_sample: 0.0,
            sample_rate: DEFAULT_SR,
            range: FrequencyRange::new(200.0, 4000.0),
        }
    }

    pub fn rate(&self) -> Arc<AtomicF32> {
        self.lfo.rate.as_atomic()
    }
    pub fn depth(&self) -> Arc<AtomicF32> {
        self.mix.depth.as_atomic()
    }
    pub fn feedback(&self) -> Arc<AtomicF32> {
        self.mix.feedback.as_atomic()
    }
    pub fn mix(&self) -> Arc<AtomicF32> {
        self.mix.mix.as_atomic()
    }

    pub fn set_rate(&self, hz: impl Into<Hz>) {
        self.lfo.rate.store(Hz(hz.into().get().max(0.01)));
    }
    pub fn set_depth(&self, d: impl Into<Linear>) {
        self.mix.depth.store(Linear(d.into().get().clamp(0.0, 1.0)));
    }
    pub fn set_feedback(&self, fb: impl Into<Linear>) {
        self.mix
            .feedback
            .store(Linear(fb.into().get().clamp(0.0, 0.99)));
    }
    pub fn set_mix(&self, mix: impl Into<Linear>) {
        self.mix.mix.store(Linear(mix.into().get().clamp(0.0, 1.0)));
    }

    pub fn set_frequency_range(&mut self, min_hz: impl Into<Hz>, max_hz: impl Into<Hz>) {
        self.range.min_hz = Hz(min_hz.into().get().max(20.0));
        self.range.max_hz = Hz(max_hz.into().get().min(self.sample_rate as f32 * 0.45));
    }

    #[inline]
    fn process_sample(&mut self, input: f32) -> f32 {
        let (depth, fb, mix) = self.mix.load();
        let sr = self.sample_rate as f32;

        // Phaser uses only the L channel of the LFO (mono effect).
        let (lfo_raw, _) = self.lfo.eval();
        let lfo = lfo_raw * 0.5 + 0.5;
        let min_hz = self.range.min_hz.get();
        let max_hz = self.range.max_hz.get();
        let sweep = min_hz + (max_hz - min_hz) * lfo * depth;

        let w = core::f32::consts::PI * sweep / sr;
        let coeff = (w.tan() - 1.0) / (w.tan() + 1.0);

        let mut sample = input + self.feedback_sample * fb;

        for stage in self.stages.iter_mut() {
            sample = stage.process(sample, coeff);
        }

        self.feedback_sample = sample;
        self.lfo.advance(tutti_core::SampleRate(self.sample_rate));

        input * (1.0 - mix) + sample * mix
    }
}

impl AudioUnit for PhaserNode {
    fn inputs(&self) -> usize {
        1
    }
    fn outputs(&self) -> usize {
        1
    }

    fn reset(&mut self) {
        for stage in &mut self.stages {
            stage.reset();
        }
        self.lfo.reset_phase();
        self.feedback_sample = 0.0;
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.sample_rate = sample_rate;
        self.range.max_hz = Hz(self.range.max_hz.get().min(sample_rate as f32 * 0.45));
    }

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        output[0] = self.process_sample(input[0]);
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        for i in 0..size {
            output.set_f32(0, i, self.process_sample(input.at_f32(0, i)));
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
        crate::node_id::PHASER_ID
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut out = SignalFrame::new(1);
        out.set(0, input.at(0));
        out
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
    }
}

impl Clone for PhaserNode {
    fn clone(&self) -> Self {
        Self {
            stages: self.stages.clone(),
            lfo: self.lfo.clone(),
            mix: self.mix.clone(),
            feedback_sample: self.feedback_sample,
            sample_rate: self.sample_rate,
            range: self.range,
        }
    }
}

/// Stereo wrapper around two independent [`PhaserNode`] instances that
/// share the same atomic parameter handles. 2-in, 2-out.
pub struct StereoPhaserNode {
    left: PhaserNode,
    right: PhaserNode,
}

impl StereoPhaserNode {
    pub fn new(stages: usize) -> Self {
        let left = PhaserNode::new(stages);
        let right = left.clone();
        Self { left, right }
    }

    pub fn rate(&self) -> Arc<AtomicF32> {
        self.left.rate()
    }
    pub fn depth(&self) -> Arc<AtomicF32> {
        self.left.depth()
    }
    pub fn feedback(&self) -> Arc<AtomicF32> {
        self.left.feedback()
    }
    pub fn mix(&self) -> Arc<AtomicF32> {
        self.left.mix()
    }

    pub fn set_rate(&self, hz: impl Into<Hz>) {
        self.left.set_rate(hz);
    }
    pub fn set_depth(&self, d: impl Into<Linear>) {
        self.left.set_depth(d);
    }
    pub fn set_feedback(&self, fb: impl Into<Linear>) {
        self.left.set_feedback(fb);
    }
    pub fn set_mix(&self, mix: impl Into<Linear>) {
        self.left.set_mix(mix);
    }
}

impl AudioUnit for StereoPhaserNode {
    fn inputs(&self) -> usize {
        2
    }
    fn outputs(&self) -> usize {
        2
    }

    fn reset(&mut self) {
        self.left.reset();
        self.right.reset();
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.left.set_sample_rate(sample_rate);
        self.right.set_sample_rate(sample_rate);
    }

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        self.left.tick(&input[0..1], &mut output[0..1]);
        self.right.tick(&input[1..2], &mut output[1..2]);
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        for i in 0..size {
            let l_in = input.at_f32(0, i);
            let r_in = input.at_f32(1, i);
            let mut l_out = [0.0f32];
            let mut r_out = [0.0f32];
            self.left.tick(&[l_in], &mut l_out);
            self.right.tick(&[r_in], &mut r_out);
            output.set_f32(0, i, l_out[0]);
            output.set_f32(1, i, r_out[0]);
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
        crate::node_id::PHASER_ID ^ 0xDA02
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut out = SignalFrame::new(2);
        out.set(0, input.at(0));
        out.set(1, input.at(1));
        out
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
    }
}

impl Clone for StereoPhaserNode {
    fn clone(&self) -> Self {
        Self {
            left: self.left.clone(),
            right: self.right.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_phaser_passthrough_dry() {
        let mut phaser = PhaserNode::new(6);
        phaser.set_sample_rate(tutti_core::SampleRate(44100.0));
        phaser.set_mix(0.0);

        let mut out = [0.0f32];
        phaser.tick(&[0.5], &mut out);
        assert!((out[0] - 0.5).abs() < 0.001);
    }

    #[test]
    fn test_phaser_produces_output() {
        let mut phaser = PhaserNode::new(6);
        phaser.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut out = [0.0f32];
        for _ in 0..1000 {
            let input = (core::f32::consts::TAU * 440.0 / 44100.0).sin();
            phaser.tick(&[input], &mut out);
        }
        assert!(out[0].abs() > 0.001, "Phaser should produce output");
    }

    #[test]
    fn test_phaser_stages_affect_sound() {
        let sr = 44100.0;
        let mut phaser_4 = PhaserNode::new(4);
        phaser_4.set_sample_rate(tutti_core::SampleRate(sr));
        phaser_4.set_mix(1.0);

        let mut phaser_12 = PhaserNode::new(12);
        phaser_12.set_sample_rate(tutti_core::SampleRate(sr));
        phaser_12.set_mix(1.0);

        let mut sum_4 = 0.0f64;
        let mut sum_12 = 0.0f64;
        let mut out = [0.0f32];

        for i in 0..4410 {
            let input = (core::f32::consts::TAU * 440.0 * i as f32 / sr as f32).sin();
            phaser_4.tick(&[input], &mut out);
            sum_4 += out[0] as f64;
            phaser_12.tick(&[input], &mut out);
            sum_12 += out[0] as f64;
        }

        assert!(
            (sum_4 - sum_12).abs() > 0.01,
            "Different stage counts should sound different"
        );
    }

    #[test]
    fn test_phaser_reset() {
        let mut phaser = PhaserNode::new(6);
        phaser.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut out = [0.0f32];
        for _ in 0..100 {
            phaser.tick(&[1.0], &mut out);
        }
        phaser.reset();
        phaser.tick(&[0.0], &mut out);
        assert!(
            out[0].abs() < 0.01,
            "After reset, output should be near zero"
        );
    }

    #[test]
    fn test_phaser_feedback_resonance() {
        let mut phaser = PhaserNode::new(6);
        phaser.set_sample_rate(tutti_core::SampleRate(44100.0));
        phaser.set_feedback(0.9);
        phaser.set_mix(1.0);

        let mut out = [0.0f32];
        phaser.tick(&[1.0], &mut out);

        let mut max_output = 0.0f32;
        for _ in 0..500 {
            phaser.tick(&[0.0], &mut out);
            max_output = max_output.max(out[0].abs());
        }
        assert!(
            max_output > 0.001,
            "High feedback should produce resonance: {max_output}"
        );
    }

    #[test]
    fn test_phaser_clamp_stages() {
        let phaser = PhaserNode::new(1);
        assert_eq!(phaser.stages.len(), 2);

        let phaser = PhaserNode::new(20);
        assert_eq!(phaser.stages.len(), MAX_STAGES);
    }
}
