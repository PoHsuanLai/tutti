//! Parameter groupings shared by modulation effects (chorus, flanger, phaser).

use tutti_core::{Hz, Linear, Param, SampleRate, Seconds};

/// LFO driver block: rate + running phase + L/R offset.
///
/// Each modulation effect advances its own phase and reads the atomic rate
/// every sample — the offset stays constant per effect (chorus: 0.25, flanger:
/// 0.5, phaser: mono) so it is a plain `f32`, not a `Param`.
#[derive(Clone)]
pub struct LfoDrive {
    pub rate: Param<Hz>,
    pub phase: f32,
    pub lr_offset: f32,
}

impl LfoDrive {
    pub fn new(rate_hz: impl Into<Hz>, lr_offset: f32) -> Self {
        Self {
            rate: Param::new(rate_hz.into()),
            phase: 0.0,
            lr_offset,
        }
    }

    #[inline]
    pub fn eval(&self) -> (f32, f32) {
        let l = (self.phase * core::f32::consts::TAU).sin();
        let r = ((self.phase + self.lr_offset) * core::f32::consts::TAU).sin();
        (l, r)
    }

    #[inline]
    pub fn advance(&mut self, sample_rate: impl Into<SampleRate>) {
        let sr = sample_rate.into().get() as f32;
        self.phase += self.rate.load().get() / sr;
        if self.phase >= 1.0 {
            self.phase -= 1.0;
        }
    }

    pub fn reset_phase(&mut self) {
        self.phase = 0.0;
    }
}

/// Wet/dry + feedback + unitless 0..1 depth (phaser-style).
///
/// Phaser uses `depth` as a unitless scalar that modulates the amplitude of
/// the LFO sweep over its all-pass frequency range.
#[derive(Clone)]
pub struct LinearModMix {
    pub depth: Param<Linear>,
    pub feedback: Param<Linear>,
    pub mix: Param<Linear>,
}

impl LinearModMix {
    pub fn new(
        depth: impl Into<Linear>,
        feedback: impl Into<Linear>,
        mix: impl Into<Linear>,
    ) -> Self {
        Self {
            depth: Param::new(depth.into()),
            feedback: Param::new(Linear(feedback.into().get().clamp(0.0, 0.99))),
            mix: Param::new(Linear(mix.into().get().clamp(0.0, 1.0))),
        }
    }

    #[inline]
    pub fn load(&self) -> (f32, f32, f32) {
        (
            self.depth.load().get(),
            self.feedback.load().get(),
            self.mix.load().get(),
        )
    }
}

/// Wet/dry + feedback + time-amplitude depth in seconds (chorus/flanger-style).
///
/// Chorus and flanger consume `depth` as the amplitude of LFO time-modulation
/// around their base delay: `delay = base + lfo * depth * sample_rate`.
#[derive(Clone)]
pub struct TimeModMix {
    pub depth: Param<Seconds>,
    pub feedback: Param<Linear>,
    pub mix: Param<Linear>,
}

impl TimeModMix {
    pub fn new(
        depth: impl Into<Seconds>,
        feedback: impl Into<Linear>,
        mix: impl Into<Linear>,
    ) -> Self {
        Self {
            depth: Param::new(depth.into()),
            feedback: Param::new(Linear(feedback.into().get().clamp(0.0, 0.99))),
            mix: Param::new(Linear(mix.into().get().clamp(0.0, 1.0))),
        }
    }

    /// Returns (depth_secs, feedback, mix) — depth carries `Seconds` semantically
    /// but unwraps to `f32` here for the per-sample math.
    #[inline]
    pub fn load(&self) -> (f32, f32, f32) {
        (
            self.depth.load().get(),
            self.feedback.load().get(),
            self.mix.load().get(),
        )
    }
}
