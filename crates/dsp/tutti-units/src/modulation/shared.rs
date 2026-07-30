//! Parameter groupings shared by modulation effects (chorus, flanger, phaser).

use tutti_core::{Depth, Feedback, Hz, Mix, Param, Phase, PhaseIncrement, SampleRate, Seconds};

/// LFO driver block: rate + running phase + L/R offset.
///
/// Each modulation effect advances its own phase and reads the atomic rate
/// every sample — the offset stays constant per effect (chorus: 0.25, flanger:
/// 0.5, phaser: mono) so it is a plain [`PhaseIncrement`], not a `Param`.
#[derive(Clone)]
pub struct LfoDrive {
    pub rate: Param<Hz>,
    pub phase: Phase,
    pub lr_offset: PhaseIncrement,
}

impl LfoDrive {
    pub fn new(rate_hz: impl Into<Hz>, lr_offset: impl Into<PhaseIncrement>) -> Self {
        Self {
            rate: Param::new(rate_hz.into()),
            phase: Phase::START,
            lr_offset: lr_offset.into(),
        }
    }

    #[inline]
    pub fn eval(&self) -> (f32, f32) {
        let l = self.phase.to_radians().get().sin();
        let r = self.phase.offset_by(self.lr_offset).to_radians().get().sin();
        (l, r)
    }

    /// Step the phase by one sample at the current rate.
    ///
    /// `per_sample` computes in f64 and narrows once; the old form divided by
    /// the sample rate already narrowed to f32, which is the drift
    /// `PhaseIncrement` exists to prevent.
    ///
    /// `Phase::advance` wraps with `rem_euclid`. What it replaced —
    /// `if phase >= 1.0 { phase -= 1.0 }` — is only a wrap when the increment
    /// is in `[0, 1)`, and nothing on this path guaranteed that. `set_rate`
    /// floors the rate at 0.01 Hz but never caps it, so a rate above the
    /// sample rate walked the phase out of `[0, 1)` permanently and froze the
    /// LFO to DC; the control-rate modulation path skips `set_rate` entirely
    /// (it writes the atomic through a caller-supplied min/max), so a negative
    /// rate ran the phase down without ever meeting the `>= 1.0` test.
    #[inline]
    pub fn advance(&mut self, sample_rate: impl Into<SampleRate>) {
        self.phase = self
            .phase
            .advance(PhaseIncrement::per_sample(self.rate.load(), sample_rate));
    }

    pub fn reset_phase(&mut self) {
        self.phase = Phase::START;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every rate this LFO can actually be given must leave the phase inside
    /// `[0, 1)`.
    ///
    /// The rates below are not hypothetical. `set_rate` floors at 0.01 Hz and
    /// never caps, and the control-rate modulation path writes the rate atomic
    /// directly with a caller-supplied min/max, so neither an absurdly high
    /// rate nor a negative one is filtered out before it reaches `advance`.
    #[test]
    fn the_phase_stays_in_range_for_every_reachable_rate() {
        let sr = SampleRate::SR_48K;
        for rate in [0.01, 2.0, 20.0, 48_000.0, 60_000.0, -2.0, -60_000.0] {
            let mut lfo = LfoDrive::new(Hz(rate), PhaseIncrement(0.25));
            for i in 0..512 {
                lfo.advance(sr);
                let p = lfo.phase.get();
                assert!(
                    (0.0..1.0).contains(&p),
                    "rate {rate} left the unit interval at step {i}: {p}"
                );
            }
        }
    }

    /// The two failure modes the old `if phase >= 1.0 { phase -= 1.0 }` had.
    #[test]
    fn a_conditional_subtract_would_not_have_wrapped_these() {
        let sr = SampleRate::SR_48K;

        // Increment > 1.0: the old form subtracted once and then climbed away
        // for good, freezing the LFO to DC.
        let mut fast = LfoDrive::new(Hz(60_000.0), PhaseIncrement(0.0));
        for _ in 0..8 {
            fast.advance(sr);
        }
        assert!((0.0..1.0).contains(&fast.phase.get()));

        // Negative rate: the old form's `>= 1.0` test never fired, so the
        // phase ran down without bound.
        let mut backward = LfoDrive::new(Hz(-2.0), PhaseIncrement(0.0));
        for _ in 0..4096 {
            backward.advance(sr);
        }
        assert!((0.0..1.0).contains(&backward.phase.get()));
    }

    /// A negative rate should run the LFO *backwards*, not merely stay in
    /// range — `rem_euclid` wrapping is what makes reverse modulation work.
    #[test]
    fn a_negative_rate_runs_the_phase_backwards() {
        let sr = SampleRate::SR_48K;
        let mut lfo = LfoDrive::new(Hz(-4800.0), PhaseIncrement(0.0));
        lfo.advance(sr); // -0.1 -> wraps to 0.9
        assert!((lfo.phase.get() - 0.9).abs() < 1e-5, "{:?}", lfo.phase);
    }

    /// The L/R offset is applied through the type, so a channel pair near the
    /// top of the cycle does not read past the end of it.
    #[test]
    fn the_lr_offset_wraps_rather_than_exceeding_one_turn() {
        let mut lfo = LfoDrive::new(Hz(1.0), PhaseIncrement(0.5));
        lfo.phase = Phase(0.7);
        // 0.7 + 0.5 = 1.2, which is 0.2 of a turn.
        let (l, r) = lfo.eval();
        assert!((l - (0.7 * core::f32::consts::TAU).sin()).abs() < 1e-6);
        assert!((r - (0.2 * core::f32::consts::TAU).sin()).abs() < 1e-5);
    }
}
