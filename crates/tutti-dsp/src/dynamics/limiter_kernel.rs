//! The lookahead limiter's per-frame gain: the curve, the instant-attack
//! release recursion on its target, before the window minimum.

use super::curves::{LimiterCurve, amplitude_to_db, compute_limiter_gain};
use super::envelope::EnvelopeFollower;
use crate::Smoother;
use tutti_types::{Amplitude, Db, SampleRate, Seconds};

/// The controls one block of a limiter runs on, read once at the top of the
/// block.
///
/// Unfed, the curve is constant across the block, so its constants (a few
/// `powf`s) are derived once here and a frame is a compare and a divide. A
/// lane replaces the threshold or the ceiling per frame, in dB; either one
/// fed leaves the curve to be evaluated per frame.
#[derive(Clone, Copy, Debug)]
pub struct LimiterBlock<'a> {
    threshold: Db,
    ceiling: Db,
    threshold_lane: Option<&'a [f32]>,
    ceiling_lane: Option<&'a [f32]>,
    curve: Option<LimiterCurve>,
}

impl<'a> LimiterBlock<'a> {
    /// A block at `threshold` and `ceiling`, either overridden per frame by
    /// its lane (in dB, indexed by the frame `i` given to
    /// [`LimiterKernel::gain`]).
    #[must_use]
    pub fn new(
        threshold: Db,
        ceiling: Db,
        threshold_lane: Option<&'a [f32]>,
        ceiling_lane: Option<&'a [f32]>,
    ) -> Self {
        let curve = (threshold_lane.is_none() && ceiling_lane.is_none())
            .then(|| LimiterCurve::new(threshold, ceiling));
        Self {
            threshold,
            ceiling,
            threshold_lane,
            ceiling_lane,
            curve,
        }
    }
}

/// A limiter's state: the gain envelope (instant attack, one-pole release)
/// and the follower that holds the release coefficient. Every control arrives
/// per call as plain values. The window minimum over the lookahead is
/// [`LookaheadRing`](super::LookaheadRing)'s.
#[derive(Clone, Debug)]
pub struct LimiterKernel {
    envelope: f32,
    follower: EnvelopeFollower,
}

impl LimiterKernel {
    /// A kernel releasing over `release` at `sample_rate`.
    #[must_use]
    pub fn new(release: Seconds, sample_rate: SampleRate) -> Self {
        Self {
            envelope: 0.0,
            follower: EnvelopeFollower::new(Seconds::new(0.0), release, sample_rate),
        }
    }

    /// Clears the envelope.
    pub fn reset(&mut self) {
        self.envelope = 0.0;
    }

    /// Retunes the release for `sample_rate`.
    pub fn set_sample_rate(&mut self, sample_rate: SampleRate, release: Seconds) {
        self.follower
            .set_sample_rate(sample_rate, Seconds(0.0), release);
    }

    /// Re-derives the release coefficient if `release` moved.
    #[inline]
    pub fn update_coefficients(&mut self, release: Seconds) {
        self.follower.update_coefficients(Seconds(0.0), release);
    }

    /// The envelope gain (linear) for the linked `peak` of frame `i` of the
    /// block: the curve's target taken instantly when it is lower, released
    /// toward it otherwise.
    #[inline]
    pub fn gain(&mut self, block: &LimiterBlock<'_>, i: usize, peak: f32) -> f32 {
        let target_gain = match &block.curve {
            Some(curve) => curve.gain(peak),
            None => {
                let peak_db = amplitude_to_db(Amplitude::new(peak));
                let threshold = block.threshold_lane.map_or(block.threshold, |v| Db(v[i]));
                let ceiling = block.ceiling_lane.map_or(block.ceiling, |v| Db(v[i]));
                compute_limiter_gain(peak_db, threshold, ceiling).get()
            }
        };
        if target_gain < self.envelope {
            self.envelope = target_gain;
        } else {
            self.envelope =
                Smoother::toward(self.envelope, target_gain, self.follower.release_coeff());
        }
        self.envelope
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: SampleRate = SampleRate(48_000.0);

    /// The attack is instant: a peak whose target is lower than the envelope
    /// takes the envelope to the target in the same frame, and the release
    /// then climbs back by the one-pole step, `e + coeff * (target - e)`.
    #[test]
    fn the_attack_is_instant_and_the_release_is_one_pole() {
        let release = Seconds(0.05);
        let mut k = LimiterKernel::new(release, RATE);
        let block = LimiterBlock::new(Db(-6.0), Db(-6.0), None, None);
        // Quiet: the gain is 1 (threshold == ceiling), the envelope rises from 0.
        let mut quiet = 0.0;
        for _ in 0..40_000 {
            quiet = k.gain(&block, 0, 0.1);
        }
        assert!((quiet - 1.0).abs() < 1e-3, "{quiet}");
        // A peak at 1.0 under a -6 dB ceiling: target 10^(-6/20).
        let target = Db(-6.0).to_amplitude().get();
        assert_eq!(k.gain(&block, 0, 1.0), target, "instant attack");
        // Back to quiet: one release step.
        let coeff = release.one_pole_coeff(RATE);
        let want = target + coeff * (1.0 - target);
        assert_eq!(k.gain(&block, 0, 0.1).to_bits(), want.to_bits());
    }

    /// A ceiling lane overrides the block's ceiling per frame, and reaches the
    /// curve at frame `i`.
    #[test]
    fn a_ceiling_lane_overrides_the_block_per_frame() {
        let lane = [-6.0f32, -12.0];
        let block = LimiterBlock::new(Db(-20.0), Db(-1.0), None, Some(&lane));
        // A zero release follows the target in one frame, so the gain is the
        // curve's own.
        let mut k = LimiterKernel::new(Seconds(0.0), RATE);
        for (i, ceiling) in lane.iter().enumerate() {
            let mut k = k.clone();
            let got = k.gain(&block, i, 1.0);
            let want = LimiterCurve::new(Db(-20.0), Db(*ceiling)).gain(1.0);
            assert!(
                (got - want).abs() <= 1e-6 * want,
                "frame {i}: {got} vs {want}"
            );
        }
        k.reset();
        assert_eq!(k.envelope, 0.0);
    }
}
