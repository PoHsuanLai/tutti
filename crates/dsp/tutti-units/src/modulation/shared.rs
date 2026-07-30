//! Parameter groupings shared by modulation effects (chorus, flanger, phaser).

use tutti_core::{Depth, Feedback, Hz, Mix, Param, SampleRate, Seconds};

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
    pub depth: Param<Depth>,
    pub feedback: Param<Feedback>,
    pub mix: Param<Mix>,
}

impl LinearModMix {
    pub fn new(
        depth: impl Into<Depth>,
        feedback: impl Into<Feedback>,
        mix: impl Into<Mix>,
    ) -> Self {
        Self {
            depth: Param::new(depth.into()),
            feedback: Param::new(Feedback::new_clamped(feedback.into().get())),
            mix: Param::new(Mix::new_clamped(mix.into().get())),
        }
    }

    /// Returns (depth, feedback, mix). `mix` stays typed — it is consumed by
    /// [`Mix::blend`] rather than by raw arithmetic; the other two feed
    /// per-sample math and unwrap here.
    #[inline]
    pub fn load(&self) -> (f32, f32, Mix) {
        (
            self.depth.load().get(),
            self.feedback.load().get(),
            self.mix.load(),
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
    pub feedback: Param<Feedback>,
    pub mix: Param<Mix>,
}

impl TimeModMix {
    pub fn new(
        depth: impl Into<Seconds>,
        feedback: impl Into<Feedback>,
        mix: impl Into<Mix>,
    ) -> Self {
        Self {
            depth: Param::new(depth.into()),
            feedback: Param::new(Feedback::new_clamped(feedback.into().get())),
            mix: Param::new(Mix::new_clamped(mix.into().get())),
        }
    }

    /// Returns (depth_secs, feedback, mix) — depth carries `Seconds`
    /// semantically but unwraps to `f32` here for the per-sample math. `mix`
    /// stays typed: it is consumed by [`Mix::blend`], not by raw arithmetic.
    #[inline]
    pub fn load(&self) -> (f32, f32, Mix) {
        (
            self.depth.load().get(),
            self.feedback.load().get(),
            self.mix.load(),
        )
    }
}
