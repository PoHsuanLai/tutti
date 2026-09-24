//! Parameter groupings shared by modulation effects (chorus, flanger, phaser).

use tutti_core::{Depth, Feedback, Hz, Mix, Param, Phase, PhaseIncrement, SampleRate, Seconds};

use crate::ramp::LastGood;

/// LFO driver block: rate + running phase.
///
/// Each modulation effect owns one and reads its rate **once per block**,
/// through [`fill_block`](Self::fill_block): the phases for the whole block
/// are computed into a buffer every channel then reads. Per-channel phase
/// offsets live on the node, not here — they are a property of the width.
#[derive(Clone)]
pub struct LfoDrive {
    pub rate: Param<Hz>,
    pub phase: Phase,
    /// The last finite rate: a NaN or ±∞ rate would make the phase NaN, and a
    /// NaN phase never wraps back (see [`LastGood`]).
    good_rate: LastGood,
}

impl LfoDrive {
    pub fn new(rate_hz: impl Into<Hz>) -> Self {
        let rate = rate_hz.into();
        Self {
            rate: Param::new(rate),
            phase: Phase::START,
            good_rate: LastGood::new(rate.get()),
        }
    }

    /// Write the phase at each sample of the block into `out` — the phase
    /// *before* that sample's step — reading the rate once, and leave the drive
    /// stepped past the block.
    ///
    /// The increment is hoisted out of the loop: the rate cannot change inside
    /// a block, so the phases are bit-identical to stepping sample by sample.
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
    pub fn fill_block(&mut self, sample_rate: impl Into<SampleRate>, out: &mut [Phase]) {
        let rate = Hz(self.good_rate.read(self.rate.load().get()));
        let inc = PhaseIncrement::per_sample(rate, sample_rate);
        for p in out {
            *p = self.phase;
            self.phase = self.phase.advance(inc);
        }
    }

    /// Step the phase by one sample at the current rate — a one-sample
    /// [`fill_block`](Self::fill_block), for the tests that walk it.
    #[cfg(test)]
    #[inline]
    pub fn advance(&mut self, sample_rate: impl Into<SampleRate>) {
        let mut one = [Phase::START];
        self.fill_block(sample_rate, &mut one);
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
    /// Last finite depth / feedback / mix (see [`LastGood`]).
    good: [LastGood; 3],
}

impl LinearModMix {
    pub fn new(
        depth: impl Into<Depth>,
        feedback: impl Into<Feedback>,
        mix: impl Into<Mix>,
    ) -> Self {
        let (depth, feedback, mix) = (
            depth.into(),
            Feedback::new_clamped(feedback.into().get()),
            Mix::new_clamped(mix.into().get()),
        );
        Self {
            depth: Param::new(depth),
            feedback: Param::new(feedback),
            mix: Param::new(mix),
            good: [
                LastGood::new(depth.get()),
                LastGood::new(feedback.get()),
                LastGood::new(mix.get()),
            ],
        }
    }

    /// Returns (depth, feedback, mix). `mix` stays typed — it is consumed by
    /// [`Mix::blend`] rather than by raw arithmetic; the other two feed
    /// per-sample math and unwrap here.
    #[inline]
    pub fn load(&mut self) -> (f32, f32, Mix) {
        // Non-finite writes read as unchanged: all three feed recursive state
        // (the sweep, the recirculation) or the output every later block ramps
        // from.
        (
            self.good[0].read(self.depth.load().get()),
            self.good[1].read(self.feedback.load().get()),
            Mix(self.good[2].read(self.mix.load().get())),
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
    /// Last finite depth / feedback / mix (see [`LastGood`]).
    good: [LastGood; 3],
}

impl TimeModMix {
    pub fn new(
        depth: impl Into<Seconds>,
        feedback: impl Into<Feedback>,
        mix: impl Into<Mix>,
    ) -> Self {
        let (depth, feedback, mix) = (
            depth.into(),
            Feedback::new_clamped(feedback.into().get()),
            Mix::new_clamped(mix.into().get()),
        );
        Self {
            depth: Param::new(depth),
            feedback: Param::new(feedback),
            mix: Param::new(mix),
            good: [
                LastGood::new(depth.get()),
                LastGood::new(feedback.get()),
                LastGood::new(mix.get()),
            ],
        }
    }

    /// Returns (depth_secs, feedback, mix) — depth carries `Seconds`
    /// semantically but unwraps to `f32` here for the per-sample math. `mix`
    /// stays typed: it is consumed by [`Mix::blend`], not by raw arithmetic.
    #[inline]
    pub fn load(&mut self) -> (f32, f32, Mix) {
        // Non-finite writes read as unchanged: all three feed recursive state
        // (the sweep, the recirculation) or the output every later block ramps
        // from.
        (
            self.good[0].read(self.depth.load().get()),
            self.good[1].read(self.feedback.load().get()),
            Mix(self.good[2].read(self.mix.load().get())),
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
            let mut lfo = LfoDrive::new(Hz(rate));
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
        let mut fast = LfoDrive::new(Hz(60_000.0));
        for _ in 0..8 {
            fast.advance(sr);
        }
        assert!((0.0..1.0).contains(&fast.phase.get()));

        // Negative rate: the old form's `>= 1.0` test never fired, so the
        // phase ran down without bound.
        let mut backward = LfoDrive::new(Hz(-2.0));
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
        let mut lfo = LfoDrive::new(Hz(-4800.0));
        lfo.advance(sr); // -0.1 -> wraps to 0.9
        assert!((lfo.phase.get() - 0.9).abs() < 1e-5, "{:?}", lfo.phase);
    }

    /// A block buffer holds the same phases as stepping sample by sample.
    ///
    /// Mutation: stepping the phase *before* writing it in `fill_block` (so
    /// `out[0]` is already one step in) fails the first assertion.
    #[test]
    fn a_block_buffer_is_the_per_sample_walk() {
        let sr = SampleRate::SR_48K;
        let mut block = LfoDrive::new(Hz(3.0));
        let mut stepped = block.clone();
        let mut phases = [Phase::START; 64];
        block.fill_block(sr, &mut phases);
        for (i, p) in phases.iter().enumerate() {
            assert_eq!(
                p.get().to_bits(),
                stepped.phase.get().to_bits(),
                "sample {i}"
            );
            stepped.advance(sr);
        }
        assert_eq!(block.phase.get().to_bits(), stepped.phase.get().to_bits());
    }
}
