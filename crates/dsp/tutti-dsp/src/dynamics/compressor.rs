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
        let input_db = amplitude_to_db(sc_level);
        let threshold_db = threshold_override
            .filter(|t| t.get().is_finite())
            .unwrap_or(block.threshold);
        let target_reduction =
            compute_compressor_gain_reduction(input_db, threshold_db, block.ratio, block.knee);
        let gain_reduction = self.follower.smooth(target_reduction.get());
        self.envelope = sc_level;
        let makeup = ramp_db(block.makeup_from, block.makeup_to, i, n);
        db_to_amplitude(-gain_reduction + makeup.get()).get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = 48_000.0;

    fn block(threshold: f32, ratio: f32) -> CompressorBlock {
        CompressorBlock {
            threshold: Db(threshold),
            knee: Db(0.0),
            ratio: CompressionRatio(ratio),
            makeup_from: Db(0.0),
            makeup_to: Db(0.0),
        }
    }

    /// A steady sidechain `over` dB above a hard-knee threshold settles to a
    /// reduction of `over * (1 - 1/ratio)` dB, and the gain applied is that
    /// reduction as an amplitude.
    ///
    /// Mutation (run): `1.0 / ratio` replaced by `ratio` in the static curve
    /// (`compute_compressor_gain_reduction`) → the 4:1 row settles at 60 dB
    /// not 7.5 and fails; the follower fed `0.0` instead of the target → the
    /// reduction stays 0 and fails.
    #[test]
    fn a_steady_level_settles_to_the_static_curve() {
        for (threshold, level_db, ratio) in [(-20.0f32, -10.0f32, 4.0f32), (-30.0, -6.0, 2.0)] {
            let mut k = CompressorKernel::new(Seconds(0.001), Seconds(0.05), RATE);
            let b = block(threshold, ratio);
            let level = db_to_amplitude(Db(level_db)).get();
            let mut gain = 1.0;
            for i in 0..20_000 {
                gain = k.gain(&b, i % 64, 64, level, None);
            }
            let want = (level_db - threshold) * (1.0 - 1.0 / ratio);
            assert!(
                (k.gain_reduction_db().get() - want).abs() < 1e-2,
                "{} vs {want}",
                k.gain_reduction_db().get()
            );
            let want_gain = db_to_amplitude(Db(-want)).get();
            assert!((gain - want_gain).abs() < 1e-3, "{gain} vs {want_gain}");
            assert_eq!(k.envelope_level().get(), level);
        }
    }

    /// A level under the threshold is not reduced.
    #[test]
    fn a_level_under_the_threshold_is_left_alone() {
        let mut k = CompressorKernel::new(Seconds(0.0), Seconds(0.0), RATE);
        let level = db_to_amplitude(Db(-40.0)).get();
        let g = k.gain(&block(-20.0, 4.0), 0, 1, level, None);
        assert_eq!(g, 1.0);
    }

    /// A non-finite per-sample threshold override falls back to the block's
    /// threshold, bit for bit; a finite one is honoured.
    ///
    /// Mutation (run): drop the `.filter(|t| t.get().is_finite())` in `gain`
    /// → a NaN threshold reaches the curve and the gain is NaN, so the
    /// equality fails.
    #[test]
    fn a_non_finite_threshold_override_falls_back_to_the_block() {
        let b = block(-20.0, 4.0);
        let level = db_to_amplitude(Db(-10.0)).get();
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut held = CompressorKernel::new(Seconds(0.0), Seconds(0.0), RATE);
            let mut fed = held.clone();
            let want = held.gain(&b, 0, 1, level, None);
            let got = fed.gain(&b, 0, 1, level, Some(Db(bad)));
            assert_eq!(got.to_bits(), want.to_bits(), "{bad}");
        }
        let mut held = CompressorKernel::new(Seconds(0.0), Seconds(0.0), RATE);
        let mut lowered = held.clone();
        let a = held.gain(&b, 0, 1, level, None);
        let c = lowered.gain(&b, 0, 1, level, Some(Db(-30.0)));
        assert!(c < a, "a lower threshold reduces more: {c} vs {a}");
    }
}
