//! The Moog-style four-stage ladder: coefficients and the per-sample step.
//!
//! Unlike the SVF this is a *nonlinear* filter: the resonance feedback runs
//! through a `tanh` saturator, so pushing resonance or drive grits and
//! compresses rather than blowing up. That saturation is the character, not a
//! safety measure.

use crate::Real;
use tutti_types::{Hz, Resonance, SampleRate};

/// Which tap of the four-stage ladder is taken as the output, and so what
/// response and slope the filter presents.
///
/// All four run the same four stages and the same feedback; they differ only in
/// which tap is read, so the type costs nothing to switch. The 24 dB variants
/// are the classic Moog sound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum LadderType {
    /// Low-pass at 12 dB/octave — the second stage tap. The default.
    #[default]
    LP12,
    /// Low-pass at 24 dB/octave — the fourth stage tap, the classic Moog
    /// response.
    LP24,
    /// High-pass at 12 dB/octave: the stage mix `u - 2*lp1 + lp2`, which is
    /// `(s / (1 + s))^2` for the stages' `1 / (1 + s)`.
    HP12,
    /// High-pass at 24 dB/octave: the binomial stage mix
    /// `u - 4*lp1 + 6*lp2 - 4*lp3 + lp4`, which is `(s / (1 + s))^4`.
    HP24,
}

/// The ladder's two coefficients, computed from its parameters.
///
/// Public for the same reason as [`SvfCoeffs`](crate::SvfCoeffs): a voice
/// engine running this topology across SIMD lanes derives its coefficients
/// from this function rather than a copy of it. The recurrence they feed is
/// the ladder node's: `u = tanh(x - k*s3)`, then four one-pole stages each
/// `v = g1*(in - s); lp = v + s; s = lp + v` with `g1 = g / (1 + g)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LadderCoeffs {
    /// Prewarped integrator gain, `tan(pi * fc / sr)`.
    pub g: f64,
    /// Feedback gain, `4 * resonance`. At `4` the ladder self-oscillates.
    pub k: f64,
}

impl LadderCoeffs {
    /// Computes ladder coefficients (pure function, no state).
    ///
    /// The cutoff is clamped to `1.0..=0.998 * Nyquist`, since `tan` diverges at
    /// Nyquist itself, and the resonance to `0.0..=1.0`.
    pub fn new(
        freq: impl Into<Hz>,
        resonance: impl Into<Resonance>,
        sample_rate: impl Into<SampleRate>,
    ) -> LadderCoeffs {
        let (freq, resonance, sample_rate) = (freq.into(), resonance.into(), sample_rate.into());
        let fc =
            f64::from(freq.get()).clamp(1.0, f64::from(sample_rate.nyquist_scaled(0.998).get()));
        LadderCoeffs {
            g: (core::f64::consts::PI * fc / sample_rate.get()).tan(),
            k: 4.0 * f64::from(resonance.get().clamp(0.0, 1.0)),
        }
    }
}

/// The ladder's two coefficients, narrowed to the state precision `F` and shared
/// by every channel.
///
/// `g1` is the one-pole stage gain `g / (1 + g)` with `g = tan(π·fc/sr)`, solved
/// once with the `tan` rather than divided out every sample.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LadderCoefficients<F: Real> {
    g1: F,
    k: F,
}

impl<F: Real> From<&LadderCoeffs> for LadderCoefficients<F> {
    /// The solve's `f64` set, with the stage gain divided out in `F`.
    fn from(c: &LadderCoeffs) -> Self {
        let g = F::from_f64(c.g);
        Self {
            g1: g / (F::from_f64(1.0) + g),
            k: F::from_f64(c.k),
        }
    }
}

impl<F: Real> LadderCoefficients<F> {
    /// Linear blend toward `to` by `t` in `0..=1`: an interpolated set is only
    /// ever used for one sample and never stored.
    #[inline]
    #[must_use]
    pub(crate) fn lerp(&self, to: &Self, t: F) -> Self {
        Self {
            g1: self.g1 + (to.g1 - self.g1) * t,
            k: self.k + (to.k - self.k) * t,
        }
    }
}

/// One channel's four-stage step over its stage words `s`.
#[inline(always)]
pub(crate) fn ladder_step<F: Real>(
    c: &LadderCoefficients<F>,
    ty: LadderType,
    s: &mut [F; 4],
    input: F,
    drive: F,
) -> F {
    let x = input * drive;
    let u = (x - c.k * s[3]).tanh();
    let g1 = c.g1;

    let v1 = g1 * (u - s[0]);
    let lp1 = v1 + s[0];
    s[0] = lp1 + v1;

    let v2 = g1 * (lp1 - s[1]);
    let lp2 = v2 + s[1];
    s[1] = lp2 + v2;

    let v3 = g1 * (lp2 - s[2]);
    let lp3 = v3 + s[2];
    s[2] = lp3 + v3;

    let v4 = g1 * (lp3 - s[3]);
    let lp4 = v4 + s[3];
    s[3] = lp4 + v4;

    match ty {
        LadderType::LP12 => lp2,
        LadderType::LP24 => lp4,
        LadderType::HP12 => u - lp1 - lp1 + lp2,
        LadderType::HP24 => {
            u - lp1 * F::from_f64(4.0) + lp2 * F::from_f64(6.0) - lp3 * F::from_f64(4.0) + lp4
        }
    }
}
