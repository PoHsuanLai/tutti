//! Shared modulated-delay core for chorus and flanger.
//!
//! The two effects are structurally identical: two delay lines modulated by
//! an LFO with some feedback and a wet/dry mix. They differ only in three
//! numeric constants (base delay, max delay, L/R phase offset) and in their
//! factory defaults — captured in [`ModulatedDelayConfig`].

use crate::delay::{DelayLine, InterpolationMode, StereoPair};
use tutti_core::dsp::DEFAULT_SAMPLE_RATE;
use tutti_core::{Feedback, Hz, Mix, PhaseIncrement, SampleRate, Seconds};

use super::shared::{LfoDrive, TimeModMix};

/// Flavor constants that distinguish one modulated-delay effect from another.
#[derive(Debug, Clone, Copy)]
pub struct ModulatedDelayConfig {
    /// Centre delay the LFO modulates around.
    pub base_delay: Seconds,
    /// Maximum allowed delay (delay-line capacity).
    pub max_delay: Seconds,
    /// L/R phase offset in LFO cycles (0..1).
    ///
    /// A [`PhaseIncrement`], not a `Seconds` — which is the point of typing
    /// this struct. All three were bare `f32`, so the two delays could be
    /// transposed (CHORUS's `0.01`/`0.05` swap to base > max, which the line
    /// then silently clamps at capacity) and the offset could be written into
    /// either of them. `LfoDrive::new` already takes
    /// `impl Into<PhaseIncrement>`; the type was stripped only to sit here.
    pub lr_phase_offset: PhaseIncrement,
}

impl ModulatedDelayConfig {
    pub const CHORUS: Self = Self {
        base_delay: Seconds(0.01),
        max_delay: Seconds(0.05),
        lr_phase_offset: PhaseIncrement(0.25),
    };

    pub const FLANGER: Self = Self {
        base_delay: Seconds(0.001),
        max_delay: Seconds(0.02),
        lr_phase_offset: PhaseIncrement(0.5),
    };
}

/// Two modulated delay lines plus wet/dry/feedback controls. The core of
/// both chorus and flanger; each wraps this with its own flavor defaults.
pub struct ModulatedDelay {
    pub delays: StereoPair<DelayLine>,
    pub lfo: LfoDrive,
    pub mix: TimeModMix,
    pub sample_rate: SampleRate,
    config: ModulatedDelayConfig,
}

impl ModulatedDelay {
    /// Builds the shared core: two delay lines sized for `config.max_delay`,
    /// an LFO at `rate_hz` and the wet/feedback/mix surface.
    ///
    /// **Starts at the placeholder [`DEFAULT_SAMPLE_RATE`]**, and both
    /// rate-dependent quantities skew together if
    /// [`set_sample_rate`](Self::set_sample_rate) is not called before the first
    /// process: the lines are *allocated* in samples from `max_delay`, and the
    /// LFO's phase increment is its rate divided by the sample rate. At 48 kHz
    /// an uncorrected core sweeps 8.8% too little delay 8.8% too slowly — a
    /// chorus that is simply shallower and lazier than configured, which is why
    /// nothing reports it. `set_sample_rate` rebuilds both lines and so
    /// reallocates. See the crate-level "born at a placeholder rate" section.
    ///
    /// [`DEFAULT_SAMPLE_RATE`]: tutti_core::dsp::DEFAULT_SAMPLE_RATE
    pub fn new(
        config: ModulatedDelayConfig,
        rate_hz: impl Into<Hz>,
        depth_secs: impl Into<Seconds>,
        feedback: impl Into<Feedback>,
        mix: impl Into<Mix>,
    ) -> Self {
        Self {
            delays: StereoPair::new(
                DelayLine::from_seconds(config.max_delay, DEFAULT_SAMPLE_RATE),
                DelayLine::from_seconds(config.max_delay, DEFAULT_SAMPLE_RATE),
            ),
            lfo: LfoDrive::new(rate_hz, config.lr_phase_offset),
            mix: TimeModMix::new(depth_secs, feedback, mix),
            sample_rate: DEFAULT_SAMPLE_RATE,
            config,
        }
    }

    pub fn reset(&mut self) {
        self.delays.l.reset();
        self.delays.r.reset();
        self.lfo.reset_phase();
    }

    pub fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.sample_rate = sample_rate;
        self.delays.l = DelayLine::from_seconds(self.config.max_delay, sample_rate);
        self.delays.r = DelayLine::from_seconds(self.config.max_delay, sample_rate);
    }

    #[inline]
    pub fn process_sample(&mut self, in_l: f32, in_r: f32, out: &mut [f32]) {
        let (depth, fb, mix) = self.mix.load();
        // Narrowed once for the three fractional delay positions below. They
        // feed an interpolated read, so they keep their fraction rather than
        // going through `Seconds::to_samples`.
        let sr = self.sample_rate.get() as f32;

        let (lfo_l, lfo_r) = self.lfo.eval();

        let base_delay = self.config.base_delay.get() * sr;
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

        out[0] = mix.blend(in_l, wet_l);
        out[1] = mix.blend(in_r, wet_r);

        self.lfo.advance(self.sample_rate);
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
