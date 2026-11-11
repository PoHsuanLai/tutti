//! State-variable filter coefficients and the per-sample step (Simper's
//! trapezoidal SVF).
//!
//! One coefficient computation serves eight responses ([`SvfType`]), so
//! switching type is as cheap as any parameter change.

use crate::Lerp;
use crate::Real;
use tutti_types::{Db, Hz, Q, SampleRate};

/// Which response the state-variable filter presents.
///
/// All eight share one coefficient computation and differ only in the output
/// mix (`m0`/`m1`/`m2`), so switching type is as cheap as any other parameter
/// change. The last three ([`Bell`](Self::Bell), [`LowShelf`](Self::LowShelf),
/// [`HighShelf`](Self::HighShelf)) are the only ones that read the gain
/// parameter; the rest ignore it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum SvfType {
    /// Passes below cutoff, attenuates above it at 12 dB/octave. The default.
    #[default]
    LowPass,
    /// Passes above cutoff, attenuates below it at 12 dB/octave.
    HighPass,
    /// Passes a band around cutoff; [`Q`] sets how narrow.
    BandPass,
    /// Rejects a band around cutoff and passes everything else — the inverse of
    /// [`BandPass`](Self::BandPass). High [`Q`] gives a narrow, deep notch.
    Notch,
    /// Allpass: flat magnitude, frequency-dependent phase shift.
    ///
    /// Passes every frequency at unity and changes only phase, which is what
    /// makes it the building block of a phaser.
    Allpass,
    /// Peaking/bell EQ: boosts or cuts a band around cutoff by the gain
    /// parameter, leaving the rest flat. [`Q`] sets the bandwidth.
    Bell,
    /// Shelf boosting or cutting everything *below* cutoff by the gain
    /// parameter, flat above it.
    LowShelf,
    /// Shelf boosting or cutting everything *above* cutoff by the gain
    /// parameter, flat below it.
    HighShelf,
}

/// Pure SVF coefficient set computed from filter parameters.
///
/// Public so a voice engine that runs the same topology across SIMD lanes
/// (`tutti-polysynth`'s voice bank) derives its coefficients from this one
/// function rather than a copy of it. The per-sample recurrence those
/// coefficients feed is `v3 = v0 - ic2; v1 = a1*ic1 + a2*v3;
/// v2 = ic2 + a2*ic1 + a3*v3; ic1 = 2*v1 - ic1; ic2 = 2*v2 - ic2;
/// y = m0*v0 + m1*v1 + m2*v2` (Simper's trapezoidal SVF).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SvfCoeffs {
    /// The prewarped integrator gain, `tan(pi * fc / sr)` (a shelf's divided or
    /// multiplied by `sqrt(A)`): where the response's frequency axis sits.
    pub g: f64,
    /// The damping, `1 / Q` (a bell's `1 / (Q * A)`).
    pub k: f64,
    /// `1 / (1 + g*(g + k))`, with `g = tan(pi * fc / sr)` and `k = 1 / Q`.
    /// A bell uses `k = 1 / (Q * A)`; a low (high) shelf divides (multiplies)
    /// `g` by `sqrt(A)`, `A = 10^(dB / 40)`.
    pub a1: f64,
    /// `g * a1`.
    pub a2: f64,
    /// `g * a2`.
    pub a3: f64,
    /// Output weight of the input `v0`.
    pub m0: f64,
    /// Output weight of the band-pass state `v1`.
    pub m1: f64,
    /// Output weight of the low-pass state `v2`.
    pub m2: f64,
}

impl SvfCoeffs {
    /// The largest gain, in dB either way, [`new`](Self::new) solves for. A
    /// gain further out is this one: 200 dB is a factor of 10^10 in
    /// amplitude, past anything audible, and keeps every coefficient finite.
    pub const MAX_GAIN_DB: f32 = 200.0;

    /// Computes SVF filter coefficients from parameters (pure function, no state).
    ///
    /// The three tuning parameters are three different units, and taking them as
    /// [`Hz`] / [`Q`] / [`Db`] is what keeps them apart. As bare `f32`s they are
    /// adjacent and interchangeable — the call reads as a run of positional numbers,
    /// `(LowPass, 1000.0, 0.707, 0.0, 44100.0)`, where transposing any two compiles
    /// and detunes the filter.
    pub fn new(
        filter_type: SvfType,
        freq: impl Into<Hz>,
        q: impl Into<Q>,
        gain_db: impl Into<Db>,
        sample_rate: impl Into<SampleRate>,
    ) -> SvfCoeffs {
        let (freq, q, gain_db) = (freq.into().get(), q.into().get(), gain_db.into().get());
        let sample_rate = sample_rate.into();
        // `tan` diverges at Nyquist itself, so the cutoff stops just short of it.
        let fc = (freq as f64).clamp(1.0, f64::from(sample_rate.nyquist_scaled(0.998).get()));
        let g0 = (core::f64::consts::PI * fc / sample_rate.get()).tan();
        let q = (q as f64).max(0.01);
        // A gain past `MAX_GAIN_DB` either way is that gain: `10^(dB / 40)`
        // underflows to 0 far out (`k = 1 / (Q * A)` is then infinite and the
        // set NaN), and overflows to infinity the other way. A NaN gain is
        // unity (0 dB).
        let gain_db = if gain_db.is_nan() {
            0.0
        } else {
            gain_db.clamp(-Self::MAX_GAIN_DB, Self::MAX_GAIN_DB)
        };
        let a = 10.0_f64.powf(gain_db as f64 / 40.0);

        // RBJ cookbook shapes: a bell's damping is `1/(Q*A)`, and a shelf's
        // midpoint (half the dB gain) sits at `fc`, so its prototype is tuned
        // `sqrt(A)` away.
        let k = match filter_type {
            SvfType::Bell => 1.0 / (q * a),
            _ => 1.0 / q,
        };
        let g = match filter_type {
            SvfType::LowShelf => g0 / a.sqrt(),
            SvfType::HighShelf => g0 * a.sqrt(),
            _ => g0,
        };

        let a1 = 1.0 / (1.0 + g * (g + k));
        let a2 = g * a1;
        let a3 = g * a2;

        let (m0, m1, m2) = match filter_type {
            SvfType::LowPass => (0.0, 0.0, 1.0),
            SvfType::HighPass => (1.0, -k, -1.0),
            SvfType::BandPass => (0.0, 1.0, 0.0),
            SvfType::Notch => (1.0, -k, 0.0),
            // Allpass: flat magnitude, all-pass phase. m1 = -2k (vs notch's -k).
            SvfType::Allpass => (1.0, -2.0 * k, 0.0),
            SvfType::Bell => (1.0, k * (a * a - 1.0), 0.0),
            SvfType::LowShelf => (1.0, k * (a - 1.0), a * a - 1.0),
            SvfType::HighShelf => (a * a, k * (1.0 - a) * a, 1.0 - a * a),
        };

        SvfCoeffs {
            g,
            k,
            a1,
            a2,
            a3,
            m0,
            m1,
            m2,
        }
    }
}

/// The coefficients of one set of controls, in the filter's state precision `F`.
///
/// One set serves every channel: the controls are one linked surface, so
/// widening replicates only the integrator state.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SvfCoefficients<F: Real> {
    a1: F,
    a2: F,
    a3: F,
    m0: F,
    m1: F,
    m2: F,
}

impl<F: Real> From<&SvfCoeffs> for SvfCoefficients<F> {
    /// The solve's `f64` set narrowed once to `F`.
    fn from(c: &SvfCoeffs) -> Self {
        Self {
            a1: F::from_f64(c.a1),
            a2: F::from_f64(c.a2),
            a3: F::from_f64(c.a3),
            m0: F::from_f64(c.m0),
            m1: F::from_f64(c.m1),
            m2: F::from_f64(c.m2),
        }
    }
}

impl<F: Real> Lerp for SvfCoefficients<F> {
    /// Each coefficient blended linearly, `t = k / n` in `F`: an interpolated set is only
    /// ever used for one sample and never stored.
    #[inline]
    fn lerp(&self, to: &Self, k: u32, n: u32) -> Self {
        let t = F::from_f64(f64::from(k)) / F::from_f64(f64::from(n));
        Self {
            a1: self.a1 + (to.a1 - self.a1) * t,
            a2: self.a2 + (to.a2 - self.a2) * t,
            a3: self.a3 + (to.a3 - self.a3) * t,
            m0: self.m0 + (to.m0 - self.m0) * t,
            m1: self.m1 + (to.m1 - self.m1) * t,
            m2: self.m2 + (to.m2 - self.m2) * t,
        }
    }
}

/// One channel's integrator step (Cytomic/Zavalishin TPT form) over its two
/// state words `[ic1eq, ic2eq]`.
///
/// The pair is kept together per channel (array-of-structures), not split into
/// one array per word: the two updates are symmetric and their loads and
/// stores stay adjacent. Measured, splitting them cost 26% at width 6 — an
/// across-channel layout only pays once a channel-inner loop is explicitly
/// vectorised, which this one is not.
#[inline(always)]
pub(crate) fn svf_step<F: Real>(c: &SvfCoefficients<F>, s: &mut [F; 2], v0: F) -> F {
    let two = F::from_f64(2.0);
    let [ic1eq, ic2eq] = *s;
    let v3 = v0 - ic2eq;
    let v1 = c.a1 * ic1eq + c.a2 * v3;
    let v2 = ic2eq + c.a2 * ic1eq + c.a3 * v3;
    *s = [two * v1 - ic1eq, two * v2 - ic2eq];
    c.m0 * v0 + c.m1 * v1 + c.m2 * v2
}

#[cfg(test)]
mod tests;
