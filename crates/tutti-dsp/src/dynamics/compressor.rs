//! The compressor's per-sample gain: the static curve, the attack/release
//! follower on the reduction, and the makeup ramp.

use super::curves::{amplitude_to_db, compute_compressor_gain_reduction, db_to_amplitude, ramp_db};
use super::envelope::EnvelopeFollower;
use tutti_types::{Amplitude, CompressionRatio, Db, SampleRate, Seconds};

/// The controls one block of a compressor runs on, read once at the top of
/// the block.
///
/// The detector-side three (threshold, knee, ratio) are held for the block:
/// they decide a *target* reduction that the attack/release follower then
/// smooths, so a step between blocks reaches the output already ramped by the
/// envelope. Makeup is not smoothed by anything, since it multiplies the
/// output directly, so it ramps linearly from `makeup_from` to `makeup_to`
/// across the block.
#[derive(Clone, Copy, Debug)]
pub struct CompressorBlock {
    /// The level above which reduction begins.
    pub threshold: Db,
    /// The soft-knee width, `0` for a hard knee.
    pub knee: Db,
    /// The compression ratio, at least 1.
    pub ratio: CompressionRatio,
    /// The makeup gain the previous block ended on.
    pub makeup_from: Db,
    /// The makeup gain this block ends on.
    pub makeup_to: Db,
}

/// A compressor's state: the sidechain level last seen and the follower that
/// smooths the reduction. Every control arrives per call as plain values.
#[derive(Clone, Debug)]
pub struct CompressorKernel {
    envelope: f32,
    follower: EnvelopeFollower,
}

impl CompressorKernel {
    /// A kernel with `attack` and `release` times at `sample_rate`.
    #[must_use]
    pub fn new(
        attack: impl Into<Seconds>,
        release: impl Into<Seconds>,
        sample_rate: impl Into<SampleRate>,
    ) -> Self {
        Self {
            envelope: 0.0,
            follower: EnvelopeFollower::new(attack, release, sample_rate),
        }
    }

    /// The reduction the follower is at now, in dB, positive.
    #[must_use]
    pub fn gain_reduction_db(&self) -> Db {
        Db(self.follower.value())
    }

    /// The sidechain peak the detector last saw. An `Amplitude`, not a
    /// unitless envelope: a gate's level is a 0..1 open-fraction, and the
    /// types are what stop the two being swapped.
    #[must_use]
    pub fn envelope_level(&self) -> Amplitude {
        Amplitude(self.envelope)
    }

    /// Clears the envelope and the follower.
    pub fn reset(&mut self) {
        self.envelope = 0.0;
        self.follower.reset();
    }

    /// Retunes the follower for `sample_rate` at `attack` and `release`.
    pub fn set_sample_rate(
        &mut self,
        sample_rate: impl Into<SampleRate>,
        attack: impl Into<Seconds>,
        release: impl Into<Seconds>,
    ) {
        self.follower.set_sample_rate(sample_rate, attack, release);
    }

    /// Re-derives the follower's coefficients if the times moved.
    #[inline]
    pub fn update_coefficients(&mut self, attack: Seconds, release: Seconds) {
        self.follower.update_coefficients(attack, release);
    }

    /// Compressor gain (linear) for frame `i` of an `n`-frame block, given the
    /// sidechain level and an optional per-sample threshold override (dB).
    /// `None` uses the block's threshold (the fast path); `Some(db)` overrides
    /// it, the audio-rate modulation path. A non-finite override falls back to
    /// the block's threshold rather than reaching the follower.
    #[inline]
    pub fn gain(
        &mut self,
        block: &CompressorBlock,
        i: usize,
        n: usize,
        sc_level: f32,
        threshold_override: Option<Db>,
    ) -> f32 {
        let input_db = amplitude_to_db(Amplitude::new(sc_level));
        let threshold_db = threshold_override
            .filter(|t| t.get().is_finite())
            .unwrap_or(block.threshold);
        let target_reduction =
            compute_compressor_gain_reduction(input_db, threshold_db, block.ratio, block.knee);
        let gain_reduction = self.follower.smooth(target_reduction.get());
        self.envelope = sc_level;
        let makeup = ramp_db(block.makeup_from, block.makeup_to, i, n);
        db_to_amplitude(Db::new(-gain_reduction + makeup.get())).get()
    }
}

#[cfg(test)]
mod tests;
