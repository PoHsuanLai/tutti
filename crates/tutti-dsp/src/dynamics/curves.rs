//! The gain curves and conversions the dynamics processors share.
//!
//! The gain-computation curves live here rather than in any one processor, so
//! a compressor and a gate pin silence at the same floor.

use tutti_types::{Amplitude, CompressionRatio, Db};

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
pub fn ramp_db(from: Db, to: Db, i: usize, n: usize) -> Db {
    if ramp_is_at_end(from, to, i, n) {
        to
    } else {
        let t = (i + 1) as f32 / n as f32;
        Db(from.get() + (to.get() - from.get()) * t)
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
pub fn amplitude_to_db(amp: impl Into<Amplitude>) -> Db {
    let amp = amp.into();
    if amp.get() <= 0.0 {
        Db::FLOOR
    } else {
        Db::from_amplitude(amp)
    }
}

/// Pure compressor gain reduction (hard/soft knee).
#[inline]
pub fn compute_compressor_gain_reduction(
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

/// Gate gain from gate level (unitless 0..1) and range in dB.
#[inline]
pub(crate) fn compute_gate_gain(gate_level: f32, range_db: impl Into<Db>) -> Amplitude {
    gate_gain_at(gate_level, range_db.into().to_amplitude().get())
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
    Db(-reduction + drive_db).to_amplitude()
}

/// Pure limiter gain computation: the transfer curve of [`LimiterCurve`] in
/// dB, for a threshold and ceiling that may change every frame.
///
/// The threshold drives the input: the gain is `drive = ceiling - threshold`
/// up to the threshold, and `ceiling / peak` past it, so the output is
/// `min(peak * drive, ceiling)`. Returns the linear gain to apply given the
/// input peak level, threshold, and ceiling (all in dB).
#[inline]
pub fn compute_limiter_gain(
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
        Db(ceiling - threshold).to_amplitude()
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
/// `ceiling - threshold` is clamped to `MAX_GAP_DB` and the threshold
/// amplitude to `f32::MIN_POSITIVE`; neither moves a threshold at or above
/// -144 dB.
#[derive(Clone, Copy, Debug)]
pub struct LimiterCurve {
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

    /// The curve for `threshold` and `ceiling`, held across a block.
    #[must_use]
    pub fn new(threshold: Db, ceiling: Db) -> Self {
        let (threshold, ceiling) = (threshold.get(), ceiling.get());
        let gap = (ceiling - threshold).min(Self::MAX_GAP_DB);
        let threshold_lin = Db(threshold).to_amplitude().get().max(f32::MIN_POSITIVE);
        Self {
            threshold: threshold_lin,
            over: Db(gap).to_amplitude().get() * threshold_lin,
            below: Db(gap).to_amplitude().get(),
        }
    }

    /// The gain for a linked peak amplitude (`>= 0`; a NaN takes `below`).
    /// It divides by `peak` only when `peak > threshold_lin`, and relies on
    /// `threshold_lin > 0` (the `MIN_POSITIVE` clamp in `new`) so that the
    /// divisor is never zero.
    #[inline]
    pub fn gain(&self, peak: f32) -> f32 {
        if peak > self.threshold {
            self.over / peak
        } else {
            self.below
        }
    }
}

#[cfg(test)]
mod tests;
