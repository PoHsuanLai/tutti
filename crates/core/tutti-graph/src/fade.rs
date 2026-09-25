//! Replacing a running node's unit with a crossfade: the [`Fade`] an
//! [`Editor::replace`](crate::Editor::replace) asks for, and the
//! [`CrossfadeCurve`] it follows.
//!
//! The rules the executor and the reference interpreter both implement (each
//! its own way — they share only the gain law below):
//!
//! - **Both units run on the same inputs** for the fade's `duration` frames,
//!   and the node's output is `incoming * g_in + outgoing * g_out`, sample by
//!   sample, with the gains of [`CrossfadeCurve::gains`]. Frame `k` of the
//!   fade (counted from the first frame of the block the commit lands on)
//!   takes `gains(k, duration)`, for `k` in `0..duration`; from frame
//!   `duration` on, the incoming unit is the node's output alone. So the fade
//!   is exactly `duration` frames long, and neither end is a step.
//! - **Events go to the incoming unit only.** The outgoing unit is handed no
//!   events and its event outputs are discarded: it only finishes what it was
//!   already sounding, the way a DAW swaps an instrument. A note it holds is
//!   faded out with it, not released.
//! - **The shapes must agree** in everything the plan was compiled from:
//!   ports, latency, in-place acceptance and event resolution. Only the tail
//!   may differ (a fading node is never skipped). A latency change is
//!   refused rather than re-aligned: the running plan's PDC is compiled for
//!   one latency, and both units must be aligned to it. Swap a unit whose
//!   latency differs with a plain [`Editor::insert`](crate::Editor::insert).
//! - **A replace while a fade runs at that key is queued**, as fundsp's
//!   `Net::crossfade` queues it: the running fade finishes, and a fade from
//!   its incoming unit to the newest one starts at the next block. Only two
//!   units ever run at once, and no swap is ever a step. A third replace
//!   while one waits supersedes the waiting unit, which never ran.
//! - **The outgoing unit retires on the control thread.** A commit that
//!   starts (or queues) a fade is held by the executor until its fade ends,
//!   then returned with the outgoing unit in it, exactly as a retired unit
//!   rides back; a held commit keeps its credit (see `Editor::commit`'s
//!   back-pressure) until then.
//! - **A hard edit cuts a fade.** Removing the key, replacing it without a
//!   fade, or a re-prepare retires every unit at the key but the newest (a
//!   re-prepare renders silence while its units are out, so there is nothing
//!   to fade).

use tutti_types::{SampleRate, Samples, Seconds};

/// The gain curve a crossfade follows.
///
/// Which to pick is a property of the two signals, not of taste:
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CrossfadeCurve {
    /// Equal **power**: a sine/cosine pair, so the summed power is constant
    /// across the fade. Right for signals with independent phase — two
    /// different sources — which add in power.
    EqualPower,
    /// Equal **amplitude**: a smooth (fifth-order) polynomial whose two halves
    /// sum to one. Right for phase-coherent signals — the same source before
    /// and after a parameter rebuild — which add in amplitude, so an
    /// equal-power fade would bump the level by up to 3 dB mid-swap.
    #[default]
    EqualAmplitude,
}

impl CrossfadeCurve {
    /// `(g_in, g_out)` for frame `k` of a fade `len` frames long, `k < len`.
    ///
    /// The fade's position is `x = (k + 1) / (len + 1)`, strictly inside
    /// `(0, 1)`: frame 0 already hears a little of the incoming unit and
    /// frame `len - 1` still a little of the outgoing one, so the fade is
    /// exactly `len` frames and the step into and out of it is one fade step,
    /// not a jump.
    ///
    /// - `EqualAmplitude`: `g_in = x³(6x² − 15x + 10)` (smoothstep's fifth
    ///   order, fundsp's `smooth5`), `g_out = 1 − g_in`.
    /// - `EqualPower`: `g_in = sin(πx/2)`, `g_out = cos(πx/2)`.
    ///
    /// This is the one piece of fade code the executor and the reference
    /// interpreter share: it is the *definition* of the curve, pinned by its
    /// own test, as the node contract is shared. Everything around it — which
    /// frame is which `k`, when a fade starts and ends, what runs — each
    /// derives on its own.
    #[inline]
    pub fn gains(self, k: usize, len: usize) -> (f32, f32) {
        debug_assert!(k < len, "frame {k} of a {len}-frame fade");
        let x = (k + 1) as f32 / (len + 1) as f32;
        match self {
            Self::EqualAmplitude => {
                let g = x * x * x * (x * (x * 6.0 - 15.0) + 10.0);
                (g, 1.0 - g)
            }
            Self::EqualPower => {
                let a = x * std::f32::consts::FRAC_PI_2;
                (a.sin(), a.cos())
            }
        }
    }
}

/// How [`Editor::replace`](crate::Editor::replace) swaps a unit: over
/// `duration` frames, along `curve`. See the `fade` module docs
/// (`src/fade.rs`) for the rules.
///
/// A zero `duration` is a plain swap: the new unit takes over at the block
/// the commit lands on, as with [`Editor::insert`](crate::Editor::insert).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fade {
    /// The fade's length, in frames at the rate the graph runs at.
    pub duration: Samples,
    /// Its gain law.
    pub curve: CrossfadeCurve,
}

impl Fade {
    /// A fade of `duration` frames.
    pub const fn new(duration: Samples, curve: CrossfadeCurve) -> Self {
        Self { duration, curve }
    }

    /// A fade of `duration` at `rate`, rounded to the nearest frame. The
    /// graph's rate is [`Editor::prepare`](crate::Editor::prepare)'s.
    pub fn seconds(duration: Seconds, rate: SampleRate, curve: CrossfadeCurve) -> Self {
        Self::new(duration.to_samples(rate), curve)
    }

    /// Whether this swaps at once.
    pub(crate) fn is_cut(&self) -> bool {
        self.duration.is_zero()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each curve keeps the law its name promises at every frame: the halves
    /// sum to one in amplitude for `EqualAmplitude` and in power for
    /// `EqualPower`; both rise monotonically from near 0 to near 1, and are
    /// strictly inside (0, 1) at both ends — so the fade is exactly `len`
    /// frames with no jump at either end.
    ///
    /// Mutation: `g_out = g_in` for equal amplitude → its sum fails. Mutation:
    /// `x = k / len` → frame 0 has `g_in = 0` → fails the strictly-inside
    /// check. Mutation: `x = (k + 1) / len` → frame `len - 1` is at `g_in = 1`
    /// → fails.
    #[test]
    fn each_curve_keeps_its_law_at_every_frame() {
        for len in [1usize, 2, 7, 240, 4800] {
            let mut prev = 0.0f32;
            for k in 0..len {
                let (a, b) = CrossfadeCurve::EqualAmplitude.gains(k, len);
                assert!((a + b - 1.0).abs() < 1e-6, "amplitude sum {len}/{k}");
                // Strictly inside at both ends. (Past a few hundred frames
                // `1 - g_in` at the last frame is below `f32`'s resolution
                // near one; the longest length checks the rest.)
                if len <= 240 {
                    assert!(a > 0.0 && a < 1.0 && b > 0.0, "inside at {len}/{k}: {a}");
                }
                // Rising, to within an `f32` rounding of the polynomial.
                assert!(a >= prev - 1e-6, "rises at {len}/{k}");
                prev = a;
                let (p, q) = CrossfadeCurve::EqualPower.gains(k, len);
                assert!((p * p + q * q - 1.0).abs() < 1e-5, "power sum {len}/{k}");
                if len <= 240 {
                    assert!(p > 0.0 && p < 1.0 && q > 0.0, "inside at {len}/{k}: {p}");
                }
            }
        }
        // Symmetric about the middle: the outgoing unit's curve is the
        // incoming one's mirror.
        let (a, _) = CrossfadeCurve::EqualAmplitude.gains(0, 9);
        let (_, b) = CrossfadeCurve::EqualAmplitude.gains(8, 9);
        assert!((a - b).abs() < 1e-6);
        assert_eq!(CrossfadeCurve::default(), CrossfadeCurve::EqualAmplitude);
    }

    /// Mutation: `to_samples_floor` → 1.5 frames becomes 1 → fails.
    #[test]
    fn seconds_round_to_the_nearest_frame() {
        let f = Fade::seconds(
            Seconds(0.005),
            SampleRate(48_000.0),
            CrossfadeCurve::EqualPower,
        );
        assert_eq!(f.duration, Samples(240));
        let f = Fade::seconds(
            Seconds(1.5 / 1000.0),
            SampleRate(1000.0),
            CrossfadeCurve::EqualPower,
        );
        assert_eq!(f.duration, Samples(2));
    }
}
