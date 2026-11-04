//! Level detection and dB conversions shared by the dynamics processors.
//!
//! The sidechain detectors and the gain-computation curves live here rather
//! than in any one processor, so a compressor and a gate measure a signal the
//! same way and pin silence at the same floor.

use tutti_core::{Amplitude, CompressionRatio, Db};
use tutti_graph::{Inputs, Outputs};

/// Max-abs sidechain detector level for each frame of a block, written to
/// `levels`, one channel at a time.
///
/// `input.get(0..ch)` are audio channels, `input.get(ch..2*ch)` sidechain
/// channels. A graph node declares all `2 * ch` inputs, and the graph feeds
/// an unconnected one silence, so every sidechain channel is there to read.
/// `levels.len()` is the block's frame count. Each frame folds its channels
/// in ascending order from 0, as a per-frame loop does, so the lane holds
/// the same values (`max` ignores a NaN either way).
#[inline]
pub(crate) fn sidechain_levels(input: &Inputs<'_>, ch: usize, levels: &mut [f32]) {
    let n = levels.len();
    levels.fill(0.0);
    for c in 0..ch {
        for (level, &x) in levels.iter_mut().zip(&input.get(ch + c)[..n]) {
            *level = level.max(x.abs());
        }
    }
}

/// Whether [`ramp_db`] returns `to` itself at frame `i`: a held value, or the
/// block's last frame.
#[inline]
pub(crate) fn ramp_is_at_end(from: Db, to: Db, i: usize, n: usize) -> bool {
    from == to || i + 1 >= n
}

/// The value at frame `i` of an `n`-frame linear ramp from `from` to `to`, in
/// dB.
///
/// Frame `i` sits `(i + 1) / n` of the way along, so the block's **last frame
/// is exactly `to`** — returned directly rather than computed, because
/// `from + (to - from) * 1.0` can round past `to`, and the next block starts
/// from `to`. A held value (`from == to`) returns `to` bit for bit at every
/// frame, which is what keeps a node whose controls never move identical to
/// one that read them per sample. A block of one (`n == 1`) takes the new
/// value at once.
#[inline]
pub(crate) fn ramp_db(from: Db, to: Db, i: usize, n: usize) -> Db {
    if ramp_is_at_end(from, to, i, n) {
        to
    } else {
        let t = (i + 1) as f32 / n as f32;
        Db(from.get() + (to.get() - from.get()) * t)
    }
}

/// Apply a per-frame gain lane to the first `ch` channels, channel-outer over
/// planar slices: `output[c][i] = input[c][i] * gains[i]`.
///
/// The dynamics processors compute one linked gain per frame (the detector is
/// sample-outer by nature — a recursive envelope) and apply it to every
/// channel; doing the apply as a slice multiply per channel is the memoryless
/// half, and the half that vectorizes.
#[inline]
pub(crate) fn apply_gain_lane(
    gains: &[f32],
    ch: usize,
    input: &Inputs<'_>,
    output: &mut Outputs<'_, '_>,
) {
    let n = gains.len();
    for c in 0..ch {
        let x = &input.get(c)[..n];
        let y = &mut output.get(c)[..n];
        for ((y, &x), &g) in y.iter_mut().zip(x).zip(gains) {
            *y = x * g;
        }
    }
}

/// Amplitude as decibels, for the dynamics detectors.
///
/// **This is where the dynamics floor is applied.** `Db::from_amplitude` gives
/// silence `-inf`, because what to substitute for it is the consumer's choice;
/// this is the consumer that chooses, so every detector pins at one floor:
/// `Db::FLOOR` is `-144`, roughly the 24-bit noise floor. Per-site floors are
/// what let this diverge — a detector at `-96` against `tutti-export`'s `-144`
/// against a third site with none. The exact value is inaudible in a
/// compressor, since anything near either floor is far below any usable
/// threshold; the agreement is the point.
///
/// The guard buys agreement, not `NaN` containment: the gain curves tolerate
/// `-inf` on their own ([`compute_compressor_gain_reduction`] sends it down the
/// `input_db <= below` branch and returns `Db(0.0)`). What it does buy is a
/// level every caller can compare, clamp and display without special-casing
/// infinity.
#[inline]
pub(crate) fn amplitude_to_db(amp: impl Into<Amplitude>) -> Db {
    let amp = amp.into();
    if amp.get() <= 0.0 {
        Db::FLOOR
    } else {
        Db::from_amplitude(amp)
    }
}

#[inline]
pub(crate) fn db_to_amplitude(db: impl Into<Db>) -> Amplitude {
    db.into().to_amplitude()
}

/// Pure compressor gain reduction (hard/soft knee).
#[inline]
pub(crate) fn compute_compressor_gain_reduction(
    input_db: impl Into<Db>,
    threshold: impl Into<Db>,
    ratio: impl Into<CompressionRatio>,
    knee: impl Into<Db>,
) -> Db {
    let input_db = input_db.into().get();
    let threshold = threshold.into().get();
    let ratio = ratio.into().get();
    let knee = knee.into().get();
    if knee <= 0.0 {
        let over_db = (input_db - threshold).max(0.0);
        Db(over_db * (1.0 - 1.0 / ratio))
    } else {
        let half_knee = knee / 2.0;
        let below = threshold - half_knee;
        let above = threshold + half_knee;

        if input_db <= below {
            Db(0.0)
        } else if input_db >= above {
            let over_db = input_db - threshold;
            Db(over_db * (1.0 - 1.0 / ratio))
        } else {
            let x = input_db - below;
            let slope = (1.0 - 1.0 / ratio) / (2.0 * knee);
            Db(slope * x * x)
        }
    }
}

/// One-pole IIR envelope smoothing: `coeff` is the fraction of the gap
/// closed ([`tutti_core::Seconds::one_pole_coeff`];
/// 1 is instant, 0 holds).
///
/// At `coeff = 1` the result is `target` only to within half an ulp of the
/// larger of the two (exactly when they are within 2× of each other). The
/// follower's `(1 − c) · current + c · target` would be exact there, but in
/// `f32` its `1 − c` rounds to 1 for a long time constant and the envelope
/// then stops approaching a lower target.
#[inline]
pub(crate) fn smooth_envelope(current: f32, target: f32, coeff: f32) -> f32 {
    current + coeff * (target - current)
}

/// Gate gain from gate level (unitless 0..1) and range in dB.
#[inline]
pub(crate) fn compute_gate_gain(gate_level: f32, range_db: impl Into<Db>) -> Amplitude {
    gate_gain_at(gate_level, db_to_amplitude(range_db).get())
}

/// [`compute_gate_gain`] with the range already converted to linear.
#[inline]
pub(crate) fn gate_gain_at(gate_level: f32, range_linear: f32) -> Amplitude {
    Amplitude(range_linear + gate_level * (1.0 - range_linear))
}

/// The limiter's gain when `peak_db` is over the threshold: the drive
/// (`drive_db = ceiling - threshold`) less the excess over the threshold,
/// which puts the driven peak at the ceiling.
#[inline]
fn limiter_over_gain(peak_db: f32, threshold: f32, drive_db: f32) -> Amplitude {
    let reduction = peak_db - threshold;
    db_to_amplitude(Db(-reduction + drive_db))
}

/// Pure limiter gain computation: the transfer curve of [`LimiterCurve`] in
/// dB, for a threshold and ceiling that may change every frame.
///
/// The threshold drives the input: the gain is `drive = ceiling - threshold`
/// up to the threshold, and `ceiling / peak` past it, so the output is
/// `min(peak * drive, ceiling)`. Returns the linear gain to apply given the
/// input peak level, threshold, and ceiling (all in dB).
#[inline]
pub(crate) fn compute_limiter_gain(
    peak_db: impl Into<Db>,
    threshold: impl Into<Db>,
    ceiling: impl Into<Db>,
) -> Amplitude {
    let peak_db = peak_db.into().get();
    let threshold = threshold.into().get();
    let ceiling = ceiling.into().get();
    if peak_db > threshold {
        limiter_over_gain(peak_db, threshold, ceiling - threshold)
    } else {
        db_to_amplitude(Db(ceiling - threshold))
    }
}

/// [`compute_limiter_gain`] for a threshold and ceiling that hold across a
/// block, evaluated on the linear peak: what depends on only those two is
/// computed once, and a frame is a compare and a divide.
///
/// # The curve
///
/// The threshold drives the input. With `drive = ceiling - threshold` (a
/// gain, in dB a difference) the gain at a linked peak is
/// `min(drive, ceiling / peak)`, so the output peak is
/// `min(peak * drive, ceiling)`: a signal under the threshold is moved by
/// the drive (a boost when the ceiling is above the threshold, a cut when
/// below), and a louder one settles at the ceiling. The two pieces meet at
/// `peak == threshold`, so the curve is continuous and monotonic whatever
/// the ordering of ceiling and threshold.
///
/// Over the threshold the dB form is `10^((drive - (peak_db - threshold)) / 20)`,
/// which is `10^(drive/20) * threshold_lin / peak`; the comparison
/// `peak_db > threshold` is `peak > threshold_lin` (the log is monotonic).
///
/// **Silence.** A zero peak is under any finite threshold, so exact digital
/// silence takes the `below` gain `10^((ceiling - threshold)/20)` (the dB
/// form pinned it at -144 dB instead, which differs only for thresholds
/// below that). To keep that boost finite (`0 * inf` is NaN), the gap
/// `ceiling - threshold` is clamped to [`Self::MAX_GAP_DB`] and the threshold
/// amplitude to `f32::MIN_POSITIVE`; neither moves a threshold at or above
/// -144 dB.
pub(crate) struct LimiterCurve {
    /// The threshold as an amplitude.
    threshold: f32,
    /// `10^(drive/20) * threshold`, the ceiling as an amplitude: the gain
    /// over the threshold is this over the peak.
    over: f32,
    /// The gain at or under the threshold.
    below: f32,
}

impl LimiterCurve {
    /// The largest `ceiling - threshold` gap used, in dB: `10^(760/20)` is
    /// 1e38, finite in `f32` (the limit is about 770 dB).
    const MAX_GAP_DB: f32 = 760.0;

    pub(crate) fn new(threshold: Db, ceiling: Db) -> Self {
        let (threshold, ceiling) = (threshold.get(), ceiling.get());
        let gap = (ceiling - threshold).min(Self::MAX_GAP_DB);
        let threshold_lin = db_to_amplitude(Db(threshold)).get().max(f32::MIN_POSITIVE);
        Self {
            threshold: threshold_lin,
            over: db_to_amplitude(Db(gap)).get() * threshold_lin,
            below: db_to_amplitude(Db(gap)).get(),
        }
    }

    /// The gain for a linked peak amplitude (`>= 0`; a NaN takes `below`).
    /// It divides by `peak` only when `peak > threshold_lin`, and relies on
    /// `threshold_lin > 0` (the `MIN_POSITIVE` clamp in `new`) so that the
    /// divisor is never zero.
    #[inline]
    pub(crate) fn gain(&self, peak: f32) -> f32 {
        if peak > self.threshold {
            self.over / peak
        } else {
            self.below
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_amplitude_db_conversion() {
        assert!((amplitude_to_db(1.0).get() - 0.0).abs() < 0.001);
        assert!((amplitude_to_db(0.5).get() - (-6.02)).abs() < 0.1);
        assert!((db_to_amplitude(0.0).get() - 1.0).abs() < 0.001);
        assert!((db_to_amplitude(-6.0).get() - 0.501).abs() < 0.01);
    }

    /// The floor lives here, not in `Db::from_amplitude`. The shared converter
    /// hands back `-inf` for silence; this is the boundary that pins it, and
    /// every dynamics detector inherits that choice by going through here.
    #[test]
    fn the_dynamics_floor_is_applied_at_this_boundary() {
        // The conversion this delegates to does NOT floor — proving the guard
        // above is doing the work rather than duplicating something upstream.
        assert!(Db::from_amplitude(Amplitude::SILENT).get().is_infinite());

        assert_eq!(amplitude_to_db(0.0), Db::FLOOR);
        assert!(amplitude_to_db(0.0).get().is_finite());
        // Negative amplitude is not physical, but a detector fed one must not
        // produce NaN either.
        assert_eq!(amplitude_to_db(-1.0), Db::FLOOR);
    }

    /// The floor's payoff: a detected *level* stays finite and comparable.
    ///
    /// Assert on the level, never on a downstream gain. The gain curves send
    /// `-inf` down their below-threshold branch and return a finite reduction,
    /// so `is_finite()` on the gain holds with the guard removed and would
    /// cover nothing.
    #[test]
    fn the_floor_keeps_a_detected_level_usable() {
        let level = amplitude_to_db(0.0);

        // Arithmetic a detector does on its own level. `-inf - -inf` is NaN,
        // which is what an unfloored level turns a difference into.
        assert!((level - Db::FLOOR).get().is_finite());
        assert!(level.get().is_finite());

        // And it stays ordered against a real threshold, so envelope
        // comparisons behave.
        assert!(level < Db(-60.0));
        assert_eq!(level.clamp(Db(-90.0), Db(0.0)), Db(-90.0));
    }

    #[test]
    fn test_compressor_gain_reduction_hard_knee() {
        // Below threshold: no reduction
        assert_eq!(
            compute_compressor_gain_reduction(-30.0, -20.0, 4.0, 0.0).get(),
            0.0
        );
        // Above threshold: ratio compression
        let gr = compute_compressor_gain_reduction(-10.0, -20.0, 4.0, 0.0).get();
        assert!((gr - 7.5).abs() < 0.01); // 10dB over * (1 - 1/4) = 7.5
    }

    #[test]
    fn test_compressor_gain_reduction_soft_knee() {
        // Well below: no reduction
        assert_eq!(
            compute_compressor_gain_reduction(-40.0, -20.0, 4.0, 6.0).get(),
            0.0
        );
        // Well above: same as hard knee
        let gr_hard = compute_compressor_gain_reduction(0.0, -20.0, 4.0, 0.0).get();
        let gr_soft = compute_compressor_gain_reduction(0.0, -20.0, 4.0, 6.0).get();
        assert!((gr_hard - gr_soft).abs() < 0.01);
        // In knee region: quadratic transition (non-zero, less than hard)
        let gr_knee = compute_compressor_gain_reduction(-20.0, -20.0, 4.0, 6.0).get();
        assert!(gr_knee > 0.0);
        assert!(gr_knee < gr_hard);
    }

    #[test]
    fn test_smooth_envelope() {
        // Attack: target > current, a tenth of the gap closed
        let result = smooth_envelope(0.0, 1.0, 0.1);
        assert!((result - 0.1).abs() < 0.001);
        // Release: target < current
        let result = smooth_envelope(1.0, 0.0, 0.1);
        assert!((result - 0.9).abs() < 0.001);
        // coeff=1: instant; coeff=0: holds
        assert_eq!(smooth_envelope(0.0, 1.0, 1.0), 1.0);
        assert_eq!(smooth_envelope(0.25, 1.0, 0.0), 0.25);
    }

    #[test]
    fn test_compute_gate_gain() {
        // Fully open (gate_level=1): gain=1.0 regardless of range
        assert!((compute_gate_gain(1.0, -80.0).get() - 1.0).abs() < 0.001);
        // Fully closed (gate_level=0): gain = range_linear
        let closed = compute_gate_gain(0.0, -80.0).get();
        assert!(closed < 0.001); // -80dB is very quiet
                                 // -12dB range, closed gate
        let closed_12 = compute_gate_gain(0.0, -12.0).get();
        assert!((closed_12 - db_to_amplitude(-12.0).get()).abs() < 0.001);
    }

    #[test]
    fn test_compute_limiter_gain() {
        // Below threshold: gain = ceiling - threshold offset
        let gain = compute_limiter_gain(-20.0, -6.0, -0.3).get();
        assert!((gain - db_to_amplitude(Db(-0.3 - (-6.0))).get()).abs() < 0.001);
        // Above threshold: reduces gain
        let gain = compute_limiter_gain(0.0, -6.0, -0.3).get();
        assert!(gain < 1.0);
    }

    /// The reviewer's measurement of the old curve at (-6, -0.3) dB, which
    /// went -60 -> -54.3, -7 -> -1.3, -6.1 -> -0.4, -5.9 -> -6.0, 0 -> -6.0:
    /// non-monotonic, with a 5.7 dB drop at the knee. With the threshold
    /// driving the input the output is `min(in + 5.7, -0.3)`, on both the
    /// dB form and the block's linear form.
    ///
    /// Mutation (run): `ceiling - threshold` -> `(ceiling - threshold).min(0.0)`
    /// in `compute_limiter_gain`, and
    /// `gap` -> `gap.min(0.0)` in `LimiterCurve::new` -> -5.9 dB comes out
    /// at -6.0 -> fails.
    #[test]
    fn the_curve_is_the_input_plus_the_drive_up_to_the_ceiling() {
        let (threshold, ceiling) = (Db(-6.0), Db(-0.3));
        let curve = LimiterCurve::new(threshold, ceiling);
        for (input, want) in [
            (-60.0f32, -54.3f32),
            (-7.0, -1.3),
            (-6.1, -0.4),
            (-5.9, -0.3),
            (0.0, -0.3),
            (12.0, -0.3),
        ] {
            let peak = db_to_amplitude(Db(input)).get();
            let block = amplitude_to_db(peak * curve.gain(peak)).get();
            let fed = amplitude_to_db(
                peak * compute_limiter_gain(amplitude_to_db(peak), threshold, ceiling).get(),
            )
            .get();
            assert!((block - want).abs() < 1e-3, "block: {input} -> {block}");
            assert!((fed - want).abs() < 1e-3, "fed: {input} -> {fed}");
        }
    }

    /// The dB form (the fed path) and the linear form (the block path) are
    /// one curve: the same gain for every peak, threshold and ceiling,
    /// boost or cut.
    ///
    /// Mutation (run): `.min(0.0)` on the gap in `LimiterCurve::new` only
    /// -> they disagree wherever the ceiling is above the threshold and the
    /// peak over it -> fails.
    #[test]
    fn the_block_and_fed_forms_are_one_curve() {
        for (t, c) in [
            (-6.0f32, -0.3f32),
            (-12.0, -1.0),
            (-3.0, -9.0),
            (-6.0, -6.0),
        ] {
            let curve = LimiterCurve::new(Db(t), Db(c));
            let mut peak_db = -80.0f32;
            while peak_db <= 12.0 {
                let peak = db_to_amplitude(Db(peak_db)).get();
                let block = curve.gain(peak);
                let fed = compute_limiter_gain(amplitude_to_db(peak), Db(t), Db(c)).get();
                assert!(
                    (block - fed).abs() <= 1e-5 * fed,
                    "({t}, {c}) at {peak_db}: {block} against {fed}"
                );
                peak_db += 0.5;
            }
        }
    }

    /// A NaN peak takes `below`, not `over / NaN`.
    ///
    /// Mutation (run): `if !(peak <= self.threshold)` in `gain` -> a NaN
    /// returns `over / NaN` -> fails.
    #[test]
    fn a_nan_peak_takes_the_below_gain() {
        let curve = LimiterCurve::new(Db(-6.0), Db(-3.0));
        assert_eq!(curve.gain(f32::NAN), curve.below);
        assert!(curve.below > 1.0);
    }
}
