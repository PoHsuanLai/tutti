//! Shared modulated-delay core for chorus and flanger.
//!
//! The two effects are structurally identical: two delay lines modulated by
//! an LFO with some feedback and a wet/dry mix. They differ only in three
//! numeric constants (base delay, max delay, L/R phase offset) and in their
//! factory defaults — captured in [`ModulatedDelayConfig`].

use crate::delay::{DelayLine, InterpolationMode, StereoPair};
use tutti_core::dsp::DEFAULT_SR;
use tutti_core::{Feedback, Hz, Mix, Seconds};

use super::shared::{LfoDrive, TimeModMix};

/// Flavor constants that distinguish one modulated-delay effect from another.
#[derive(Debug, Clone, Copy)]
pub struct ModulatedDelayConfig {
    /// Centre delay, in seconds, that the LFO modulates around.
    pub base_delay_secs: f32,
    /// Maximum allowed delay (delay-line capacity), in seconds.
    pub max_delay_secs: f32,
    /// L/R phase offset in LFO cycles (0..1).
    pub lr_phase_offset: f32,
}

impl ModulatedDelayConfig {
    pub const CHORUS: Self = Self {
        base_delay_secs: 0.01,
        max_delay_secs: 0.05,
        lr_phase_offset: 0.25,
    };

    pub const FLANGER: Self = Self {
        base_delay_secs: 0.001,
        max_delay_secs: 0.02,
        lr_phase_offset: 0.5,
    };
}

/// Two modulated delay lines plus wet/dry/feedback controls. The core of
/// both chorus and flanger; each wraps this with its own flavor defaults.
pub struct ModulatedDelay {
    pub delays: StereoPair<DelayLine>,
    pub lfo: LfoDrive,
    pub mix: TimeModMix,
    pub sample_rate: f64,
    config: ModulatedDelayConfig,
}

impl ModulatedDelay {
    pub fn new(
        config: ModulatedDelayConfig,
        rate_hz: impl Into<Hz>,
        depth_secs: impl Into<Seconds>,
        feedback: impl Into<Feedback>,
        mix: impl Into<Mix>,
    ) -> Self {
        Self {
            delays: StereoPair::new(
                DelayLine::from_seconds(config.max_delay_secs, DEFAULT_SR),
                DelayLine::from_seconds(config.max_delay_secs, DEFAULT_SR),
            ),
            lfo: LfoDrive::new(rate_hz, config.lr_phase_offset),
            mix: TimeModMix::new(depth_secs, feedback, mix),
            sample_rate: DEFAULT_SR,
            config,
        }
    }

    pub fn reset(&mut self) {
        self.delays.l.reset();
        self.delays.r.reset();
        self.lfo.reset_phase();
    }

    pub fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.sample_rate = sample_rate;
        self.delays.l = DelayLine::from_seconds(self.config.max_delay_secs, sample_rate);
        self.delays.r = DelayLine::from_seconds(self.config.max_delay_secs, sample_rate);
    }

    pub fn base_delay_secs(&self) -> f32 {
        self.config.base_delay_secs
    }

    #[inline]
    pub fn process_sample(&mut self, in_l: f32, in_r: f32, out: &mut [f32]) {
        let (depth, fb, mix) = self.mix.load();
        let sr = self.sample_rate as f32;

        let (lfo_l, lfo_r) = self.lfo.eval();

        let base_delay = self.config.base_delay_secs * sr;
        let delay_l = (base_delay + lfo_l * depth * sr).max(1.0);
        let delay_r = (base_delay + lfo_r * depth * sr).max(1.0);

        let fb_l = self
            .delays
            .l
            .read_sample(delay_l, InterpolationMode::Linear);
        let fb_r = self
            .delays
            .r
            .read_sample(delay_r, InterpolationMode::Linear);

        self.delays.l.push_sample(in_l + fb_l * fb);
        self.delays.r.push_sample(in_r + fb_r * fb);

        let wet_l = self
            .delays
            .l
            .read_sample(delay_l, InterpolationMode::Linear);
        let wet_r = self
            .delays
            .r
            .read_sample(delay_r, InterpolationMode::Linear);

        out[0] = in_l * (1.0 - mix) + wet_l * mix;
        out[1] = in_r * (1.0 - mix) + wet_r * mix;

        self.lfo.advance(tutti_core::SampleRate(self.sample_rate));
    }

    pub fn footprint(&self) -> usize {
        (self.delays.l.buffer.len() + self.delays.r.buffer.len()) * core::mem::size_of::<f32>()
    }
}

impl Clone for ModulatedDelay {
    fn clone(&self) -> Self {
        Self {
            delays: self.delays.clone(),
            lfo: self.lfo.clone(),
            mix: self.mix.clone(),
            sample_rate: self.sample_rate,
            config: self.config,
        }
    }
}
