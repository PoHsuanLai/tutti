//! State-variable filter coefficients and the per-sample step (Simper's
//! trapezoidal SVF).
//!
//! One coefficient computation serves eight responses ([`SvfType`]), so
//! switching type is as cheap as any parameter change.

use super::Real;
use tutti_types::{Db, Hz, Q, SampleRate};

/// Below these deltas a freq/Q/gain change doesn't warrant recomputing the
/// coefficients — the change guard shared by the atomic and modulation paths.
///
/// The thresholds alone would not keep a NaN out (`|NaN - last| > eps` is
/// false, so a NaN is held off only until another control moves); the cells
/// are read through `LastGood`, which does.
const FREQ_EPS: f32 = 0.01;
const Q_EPS: f32 = 0.0001;
const GAIN_EPS: f32 = 0.01;

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

/// Parameter-derived filter coefficients, plus the parameter values they were
/// solved at — which is what gates recomputation and where a block ramp starts.
///
/// One set serves every channel: the params are one linked control surface, so
/// widening replicates only the integrator state.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SvfCoefficients<F: Real> {
    a1: F,
    a2: F,
    a3: F,
    m0: F,
    m1: F,
    m2: F,
    /// The cutoff this set was solved at.
    pub last_freq: Hz,
    /// The resonance this set was solved at.
    pub last_q: Q,
    /// The gain this set was solved at.
    pub last_gain_db: Db,
}

impl<F: Real> SvfCoefficients<F> {
    /// The set solved at `freq`, `q` and `gain_db` for `sample_rate`.
    #[must_use]
    pub fn solve(
        filter_type: SvfType,
        freq: Hz,
        q: Q,
        gain_db: Db,
        sample_rate: SampleRate,
    ) -> Self {
        Self::from_coeffs(
            &SvfCoeffs::new(filter_type, freq, q, gain_db, sample_rate),
            freq,
            q,
            gain_db,
        )
    }

    /// The set of `c`, which was solved at `freq`, `q` and `gain_db`.
    #[must_use]
    pub fn from_coeffs(c: &SvfCoeffs, freq: Hz, q: Q, gain_db: Db) -> Self {
        Self {
            a1: F::from_f64(c.a1),
            a2: F::from_f64(c.a2),
            a3: F::from_f64(c.a3),
            m0: F::from_f64(c.m0),
            m1: F::from_f64(c.m1),
            m2: F::from_f64(c.m2),
            last_freq: freq,
            last_q: q,
            last_gain_db: gain_db,
        }
    }

    /// Marks the set stale, so the next read solves at its target outright
    /// rather than ramping from parameters that no longer mean anything (a new
    /// sample rate, a new response type).
    pub fn invalidate(&mut self) {
        self.last_freq = Hz(-1.0);
    }

    /// Whether the set was marked stale by [`invalidate`](Self::invalidate).
    #[must_use]
    #[inline]
    pub fn is_invalid(&self) -> bool {
        self.last_freq.get() < 0.0
    }

    /// Whether `(freq, q, gain_db)` moved past the epsilons since this set was
    /// solved. A held value costs nothing.
    #[inline]
    #[must_use]
    pub fn moved(&self, freq: Hz, q: Q, gain_db: Db) -> bool {
        (freq.get() - self.last_freq.get()).abs() > FREQ_EPS
            || (q.get() - self.last_q.get()).abs() > Q_EPS
            || (gain_db.get() - self.last_gain_db.get()).abs() > GAIN_EPS
    }

    /// Linear blend toward `to` by `t` in `0..=1`. The parameters it was solved
    /// at are not blended — an interpolated set is only ever used for one
    /// sample and never stored.
    #[inline]
    #[must_use]
    pub fn lerp(&self, to: &Self, t: F) -> Self {
        Self {
            a1: self.a1 + (to.a1 - self.a1) * t,
            a2: self.a2 + (to.a2 - self.a2) * t,
            a3: self.a3 + (to.a3 - self.a3) * t,
            m0: self.m0 + (to.m0 - self.m0) * t,
            m1: self.m1 + (to.m1 - self.m1) * t,
            m2: self.m2 + (to.m2 - self.m2) * t,
            ..*to
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
pub fn svf_step<F: Real>(c: &SvfCoefficients<F>, s: &mut [F; 2], v0: F) -> F {
    let two = F::from_f64(2.0);
    let [ic1eq, ic2eq] = *s;
    let v3 = v0 - ic2eq;
    let v1 = c.a1 * ic1eq + c.a2 * v3;
    let v2 = ic2eq + c.a2 * ic1eq + c.a3 * v3;
    *s = [two * v1 - ic1eq, two * v2 - ic2eq];
    c.m0 * v0 + c.m1 * v1 + c.m2 * v2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_svf_coeffs_lowpass_m_values() {
        let c = SvfCoeffs::new(
            SvfType::LowPass,
            Hz::new(1000.0),
            Q::new(0.707),
            Db::new(0.0),
            SampleRate::new(44100.0),
        );
        assert!((c.m0 - 0.0).abs() < 1e-10);
        assert!((c.m1 - 0.0).abs() < 1e-10);
        assert!((c.m2 - 1.0).abs() < 1e-10);
        assert!(c.a1 > 0.0 && c.a1 < 1.0);
    }

    #[test]
    fn test_svf_coeffs_highpass_m_values() {
        let c = SvfCoeffs::new(
            SvfType::HighPass,
            Hz::new(1000.0),
            Q::new(0.707),
            Db::new(0.0),
            SampleRate::new(44100.0),
        );
        assert!((c.m0 - 1.0).abs() < 1e-10);
        assert!((c.m2 - (-1.0)).abs() < 1e-10);
    }

    /// The RBJ cookbook biquad's magnitude (dB) at `f`, closed form.
    fn rbj_db(ty: SvfType, fc: f64, q: f64, gain_db: f64, rate: f64, f: f64) -> f64 {
        let a = 10f64.powf(gain_db / 40.0);
        let w0 = 2.0 * core::f64::consts::PI * fc / rate;
        let (sn, cs) = w0.sin_cos();
        let al = sn / (2.0 * q);
        let sa = 2.0 * a.sqrt() * al;
        let (b, d) = match ty {
            SvfType::Bell => (
                [1.0 + al * a, -2.0 * cs, 1.0 - al * a],
                [1.0 + al / a, -2.0 * cs, 1.0 - al / a],
            ),
            SvfType::LowShelf => (
                [
                    a * ((a + 1.0) - (a - 1.0) * cs + sa),
                    2.0 * a * ((a - 1.0) - (a + 1.0) * cs),
                    a * ((a + 1.0) - (a - 1.0) * cs - sa),
                ],
                [
                    (a + 1.0) + (a - 1.0) * cs + sa,
                    -2.0 * ((a - 1.0) + (a + 1.0) * cs),
                    (a + 1.0) + (a - 1.0) * cs - sa,
                ],
            ),
            SvfType::HighShelf => (
                [
                    a * ((a + 1.0) + (a - 1.0) * cs + sa),
                    -2.0 * a * ((a - 1.0) + (a + 1.0) * cs),
                    a * ((a + 1.0) + (a - 1.0) * cs - sa),
                ],
                [
                    (a + 1.0) - (a - 1.0) * cs + sa,
                    2.0 * ((a - 1.0) - (a + 1.0) * cs),
                    (a + 1.0) - (a - 1.0) * cs - sa,
                ],
            ),
            _ => unreachable!(),
        };
        let w = 2.0 * core::f64::consts::PI * f / rate;
        let mag = |c: [f64; 3]| {
            let re = c[0] + c[1] * w.cos() + c[2] * (2.0 * w).cos();
            let im = -(c[1] * w.sin() + c[2] * (2.0 * w).sin());
            (re * re + im * im).sqrt()
        };
        20.0 * (mag(b) / mag(d)).log10()
    }

    /// Steady-state gain (dB) of a unit sine at `f` through the filter's own
    /// recurrence, projected on the last whole second.
    fn measured_db(ty: SvfType, fc: f64, q: f64, gain_db: f64, rate: f64, f: f64) -> f64 {
        let c = SvfCoefficients::<f64>::solve(
            ty,
            Hz(fc as f32),
            Q(q as f32),
            Db(gain_db as f32),
            SampleRate::new(rate),
        );
        let mut st = [0.0f64; 2];
        let n = rate as usize;
        let (mut re, mut im) = (0.0, 0.0);
        for i in 0..2 * n {
            let ph = 2.0 * core::f64::consts::PI * f * i as f64 / rate;
            let y = svf_step(&c, &mut st, ph.sin());
            if i >= n {
                re += y * ph.sin();
                im += y * ph.cos();
            }
        }
        20.0 * (2.0 * (re * re + im * im).sqrt() / n as f64).log10()
    }

    /// Bell and both shelves follow the RBJ cookbook response at fc/2, fc and
    /// 2fc, for +-12 dB and Q 0.7, 1, 4; the shelf gain at fc is half the dB
    /// gain.
    ///
    /// Mutations (run): Bell `k = 1/q`, shelf `g = g0`; each fails here.
    #[test]
    fn bell_and_shelves_match_the_rbj_cookbook() {
        let (rate, fc) = (48000.0, 1000.0);
        for ty in [SvfType::Bell, SvfType::LowShelf, SvfType::HighShelf] {
            for gain in [12.0, -12.0] {
                for q in [0.7, 1.0, 4.0] {
                    for f in [fc / 2.0, fc, 2.0 * fc] {
                        let want = rbj_db(ty, fc, q, gain, rate, f);
                        let got = measured_db(ty, fc, q, gain, rate, f);
                        assert!(
                            (got - want).abs() < 0.05,
                            "{ty:?} {gain} dB Q {q} at {f} Hz: got {got:.3}, RBJ {want:.3}"
                        );
                    }
                }
            }
        }
        for ty in [SvfType::LowShelf, SvfType::HighShelf] {
            let at_fc = measured_db(ty, fc, core::f64::consts::FRAC_1_SQRT_2, 12.0, rate, fc);
            assert!((at_fc - 6.0).abs() < 0.05, "{ty:?} at fc: {at_fc}");
        }
    }

    /// A gain far outside any audible range, or not a number, still solves to
    /// a finite set (it was NaN at -1e30 dB: `10^(dB / 40)` underflows to 0
    /// and `k = 1 / (Q * A)` is infinite), and a gain past the cap is the cap's.
    ///
    /// Mutation (run): dropping the clamp makes the `-1e30` and `1e30` cases
    /// non-finite; dropping the NaN arm makes the NaN case NaN.
    #[test]
    fn an_absurd_gain_solves_finite_and_saturates() {
        for ty in [SvfType::Bell, SvfType::LowShelf, SvfType::HighShelf] {
            for db in [f32::NAN, -1e30, 1e30, f32::NEG_INFINITY, f32::INFINITY] {
                let c = SvfCoeffs::new(
                    ty,
                    Hz::new(1000.0),
                    Q::new(1.0),
                    Db::new(db),
                    SampleRate::new(48_000.0),
                );
                for v in [c.a1, c.a2, c.a3, c.m0, c.m1, c.m2] {
                    assert!(v.is_finite(), "{ty:?} at {db} dB: {c:?}");
                }
            }
            let cap = SvfCoeffs::new(
                ty,
                Hz::new(1000.0),
                Q::new(1.0),
                Db::new(SvfCoeffs::MAX_GAIN_DB),
                SampleRate::new(48_000.0),
            );
            let past = SvfCoeffs::new(
                ty,
                Hz::new(1000.0),
                Q::new(1.0),
                Db::new(1e30),
                SampleRate::new(48_000.0),
            );
            assert_eq!((cap.m0, cap.m1, cap.m2), (past.m0, past.m1, past.m2));
        }
        let nan = SvfCoeffs::new(
            SvfType::Bell,
            Hz::new(1000.0),
            Q::new(1.0),
            Db::new(f32::NAN),
            SampleRate::new(48_000.0),
        );
        let flat = SvfCoeffs::new(
            SvfType::Bell,
            Hz::new(1000.0),
            Q::new(1.0),
            Db::new(0.0),
            SampleRate::new(48_000.0),
        );
        assert_eq!(nan.m1, flat.m1);
    }

    #[test]
    fn test_svf_coeffs_bell_zero_gain_is_unity() {
        let c = SvfCoeffs::new(
            SvfType::Bell,
            Hz::new(1000.0),
            Q::new(1.0),
            Db::new(0.0),
            SampleRate::new(44100.0),
        );
        assert!(
            (c.m1).abs() < 1e-10,
            "Bell with 0dB gain should have m1≈0, got {}",
            c.m1
        );
    }

    /// Cutoff and Q are not interchangeable, and the types are what say so.
    ///
    /// As adjacent `f32`s, `(freq, q)` and `(q, freq)` both compile and the
    /// second silently detunes the filter. Passing them as `Hz` and `Q` makes
    /// the transposition a type error — this pins the difference the swap would
    /// make, so the two are never quietly given one type again.
    #[test]
    fn cutoff_and_q_are_not_interchangeable() {
        let right = SvfCoeffs::new(
            SvfType::LowPass,
            Hz(1000.0),
            Q(0.707),
            Db(0.0),
            SampleRate::new(44100.0),
        );
        // What the transposed call computes: a 0.707 Hz cutoff at Q 1000.
        let swapped = SvfCoeffs::new(
            SvfType::LowPass,
            Hz(0.707),
            Q(1000.0),
            Db(0.0),
            SampleRate::new(44100.0),
        );
        assert!(
            (right.a2 - swapped.a2).abs() > 1e-6,
            "the swap has to be observable, or this test proves nothing"
        );
    }

    #[test]
    fn test_svf_coeffs_deterministic() {
        let c1 = SvfCoeffs::new(
            SvfType::LowPass,
            Hz::new(500.0),
            Q::new(0.707),
            Db::new(0.0),
            SampleRate::new(44100.0),
        );
        let c2 = SvfCoeffs::new(
            SvfType::LowPass,
            Hz::new(500.0),
            Q::new(0.707),
            Db::new(0.0),
            SampleRate::new(44100.0),
        );
        assert_eq!(c1.a1, c2.a1);
        assert_eq!(c1.a2, c2.a2);
        assert_eq!(c1.m0, c2.m0);
    }

    // =========================================================================
    // Width
    // =========================================================================
}
