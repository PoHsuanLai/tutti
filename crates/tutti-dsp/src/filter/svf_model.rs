//! The state-variable filter as a model (design 024 step 2a): [`SvfModel`] is the
//! system [`SvfControls`] describe, an [`Lti`] with one input and one output.
//!
//! The model is a transfer function and nothing else: its response is read from
//! the coefficients' `g` and `k` (the analog prototype `1 / (s^2 + k s + 1)` under
//! the bilinear map), never from a realization. A transposed direct form II and a
//! lattice compute the same `H(z)` (`Biquad`), which is what makes the model the
//! equivalence class and any realization one member of it.

use core::f64::consts::PI;

use tutti_types::{Db, Hz, Q, SampleRate, Samples, Seconds};

use super::{Biquad, SvfCoeffs, SvfType};
use crate::filter::biquad::section_l1;
use crate::{Complex64, Lti, Ring, System};

/// What an SVF's controls are: its response, cutoff, resonance and the gain of a
/// bell or shelf (read only by those three).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SvfControls {
    /// The response.
    pub ty: SvfType,
    /// The cutoff, centre or corner.
    pub freq: Hz,
    /// The resonance.
    pub q: Q,
    /// The gain of a bell or a shelf.
    pub gain: Db,
}

impl SvfControls {
    /// The largest `Q` a design asks the coefficients for. Past it the poles are
    /// indistinguishable from the unit circle in `f64` at a low cutoff, and a model
    /// whose radius rounds to 1 could not be called stable.
    pub const MAX_Q: f32 = 1_000.0;

    /// The controls a non-finite value falls back to: a 1 kHz Butterworth
    /// low-pass at 0 dB.
    pub const DEFAULT: SvfControls = SvfControls {
        ty: SvfType::LowPass,
        freq: Hz(1_000.0),
        q: Q::BUTTERWORTH,
        gain: Db(0.0),
    };

    /// Whether these controls are past the dead-band of `applied`, the controls a
    /// model was last designed from: a response change, or a cutoff, resonance or
    /// gain past `FREQ_EPS`, `Q_EPS` or `GAIN_EPS`. Below them a change does not
    /// warrant a new design, so a held value costs nothing.
    ///
    /// The bands alone would not keep a NaN out (`|NaN - last| > eps` is false, so a
    /// NaN is held off only until another control moves); a node reads its cells
    /// through `LastGood`, which does.
    #[must_use]
    pub fn moved_from(&self, applied: &Self) -> bool {
        const FREQ_EPS: f32 = 0.01;
        const Q_EPS: f32 = 0.0001;
        const GAIN_EPS: f32 = 0.01;
        self.ty != applied.ty
            || (self.freq.get() - applied.freq.get()).abs() > FREQ_EPS
            || (self.q.get() - applied.q.get()).abs() > Q_EPS
            || (self.gain.get() - applied.gain.get()).abs() > GAIN_EPS
    }

    /// These controls with every non-finite figure replaced by the default's and
    /// `q` held in `0.01..=MAX_Q`. A cutoff and a gain out of range are
    /// [`SvfCoeffs::new`]'s to clamp.
    fn sanitized(self) -> Self {
        let finite = |v: f32, d: f32| if v.is_finite() { v } else { d };
        Self {
            ty: self.ty,
            freq: Hz(finite(self.freq.get(), Self::DEFAULT.freq.get())),
            q: Q(finite(self.q.get(), Self::DEFAULT.q.get()).clamp(0.01, Self::MAX_Q)),
            gain: Db(if self.gain.get().is_nan() {
                0.0
            } else {
                self.gain.get()
            }),
        }
    }
}

/// The system an [`SvfControls`] describes at a rate: a two-pole, two-zero
/// section, an [`Lti`] with one input and one output.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SvfModel {
    coeffs: SvfCoeffs,
    controls: SvfControls,
    rate: SampleRate,
}

impl SvfModel {
    /// The model of `controls` at `rate`. Total: a non-finite control takes the
    /// default's and the rest are clamped as the coefficients clamp them.
    #[must_use]
    pub fn new(controls: SvfControls, rate: SampleRate) -> Self {
        let controls = controls.sanitized();
        Self {
            coeffs: SvfCoeffs::new(controls.ty, controls.freq, controls.q, controls.gain, rate),
            controls,
            rate,
        }
    }

    /// The controls it is the model of (sanitised).
    #[must_use]
    pub fn controls(&self) -> SvfControls {
        self.controls
    }

    /// The coefficients of the trapezoidal form, which carry `g`, `k` and the
    /// output weights.
    #[must_use]
    pub fn coeffs(&self) -> &SvfCoeffs {
        &self.coeffs
    }

    /// The rate it is designed at.
    #[must_use]
    pub fn rate(&self) -> SampleRate {
        self.rate
    }

    /// The same `H(z)` as direct-form coefficients: what a transposed direct form
    /// II or a lattice realizes. Under `s = (z - 1) / (g (z + 1))` the denominator
    /// `s^2 + k s + 1` times `g^2 (z + 1)^2` is `d0 z^2 + d1 z + d2` with
    /// `d0 = 1 + g (g + k)`, `d1 = 2 g^2 - 2` and `d2 = 1 - g (k - g)`; the numerator
    /// `m0 + (m1 s + m2) / D` is `m0` times that, plus `m1 g (z^2 - 1)` and
    /// `m2 g^2 (z + 1)^2`; both are divided by `d0`.
    #[must_use]
    pub fn biquad(&self) -> Biquad {
        let SvfCoeffs {
            g, k, m0, m1, m2, ..
        } = self.coeffs;
        let d0 = 1.0 + g * (g + k);
        let d1 = 2.0 * g * g - 2.0;
        let d2 = 1.0 - g * k + g * g;
        Biquad {
            b0: (m0 * d0 + m1 * g + m2 * g * g) / d0,
            b1: (m0 * d1 + 2.0 * m2 * g * g) / d0,
            b2: (m0 * d2 - m1 * g + m2 * g * g) / d0,
            a1: d1 / d0,
            a2: d2 / d0,
        }
    }

    /// The two poles, from the prototype's roots of `s^2 + k s + 1` through
    /// `z = (1 + g s) / (1 - g s)`. The roots are found in the cancellation-free
    /// form (a real pair `q` and `1 / q`).
    #[must_use]
    pub fn poles(&self) -> [Complex64; 2] {
        let (g, k) = (self.coeffs.g, self.coeffs.k);
        let map = |s: Complex64| (1.0 + g * s) / (1.0 - g * s);
        let disc = k * k - 4.0;
        if disc < 0.0 {
            let s = Complex64::new(-k / 2.0, (-disc).sqrt() / 2.0);
            [map(s), map(s.conj())]
        } else {
            let fast = -(k + disc.sqrt()) / 2.0;
            [
                map(Complex64::new(fast, 0.0)),
                map(Complex64::new(1.0 / fast, 0.0)),
            ]
        }
    }

    /// The slowest pole's `-ln |z|`, per frame, to full precision however close
    /// the pole is to the unit circle (`1 - |z|` is formed without cancelling).
    fn decay_rate(&self) -> f64 {
        let (g, k) = (self.coeffs.g, self.coeffs.k);
        let disc = k * k - 4.0;
        if disc < 0.0 {
            // |z|^2 = (1 - g k + g^2) / (1 + g k + g^2) for a conjugate pair.
            -0.5 * libm::log1p(-2.0 * g * k / (1.0 + g * k + g * g))
        } else {
            // The slow real root, |s| = 1 / |q|, |z| = |1 - g |s|| / (1 + g |s|).
            let s = 2.0 / (k + disc.sqrt());
            let gs = g * s;
            if gs < 1.0 {
                -libm::log1p(-2.0 * gs / (1.0 + gs))
            } else {
                -libm::log((gs - 1.0) / (gs + 1.0))
            }
        }
    }

    /// `|H(e^jw)|` and the response itself, from the trapezoidal form (accurate
    /// down to DC: no cancellation between the polynomial's terms).
    fn at(&self, w: f64) -> Complex64 {
        let SvfCoeffs {
            g, k, m0, m1, m2, ..
        } = self.coeffs;
        // The warped frequency in units of the prototype's cutoff: tan(w/2) / g.
        let t = libm::tan(w / 2.0) / g;
        let d = Complex64::new(1.0 - t * t, k * t);
        m0 + Complex64::new(m2, m1 * t) / d
    }

    /// `g_n`, the impulse response of the all-pole part `1 / (1 + a1 z^-1 + a2
    /// z^-2)`, in closed form from the poles.
    fn all_pole(&self, n: i64) -> f64 {
        if n < 0 {
            return 0.0;
        }
        let [p, q] = self.poles();
        let m = (n + 1) as f64;
        if p.im.abs() > 0.0 {
            let (r, th) = (p.norm(), p.arg());
            return libm::pow(r, n as f64) * libm::sin(m * th) / libm::sin(th);
        }
        let (a, b) = (p.re, q.re);
        let pow = |x: f64, e: f64| {
            let sign = if x < 0.0 && (e as i64) % 2 != 0 {
                -1.0
            } else {
                1.0
            };
            sign * libm::pow(x.abs(), e)
        };
        if (a - b).abs() >= 1e-9 * a.abs().max(b.abs()) {
            (pow(a, m) - pow(b, m)) / (a - b)
        } else {
            m * pow(0.5 * (a + b), n as f64)
        }
    }

    /// The envelope constant `K` of `|g_n| <= min(n + 1, K) r^n`.
    fn envelope_k(&self) -> f64 {
        let [p, q] = self.poles();
        let gap = (p - q).norm();
        if gap > 0.0 {
            2.0 * p.norm().max(q.norm()) / gap
        } else {
            f64::INFINITY
        }
    }
}

impl System for SvfModel {
    /// Zero: no lookahead, and the filter's response starts at its first frame.
    fn latency(&self) -> Samples {
        Samples::ZERO
    }

    /// The frames until `|h[n]|` is at most `10^(floor / 20)` of its largest, from
    /// the envelope `(|b0| + |b1| + |b2|) min(n - 1, K) r^(n - 2)` of the numerator over the
    /// all-pole part (`r` the slowest pole, `K = 2r / |p1 - p2|`) against the
    /// largest `|h|` found on a geometric grid and refined at its peak, so it covers
    /// the true ring and sits within a small factor of it.
    fn tail(&self, floor: Db) -> Ring {
        let nln = self.decay_rate();
        if nln.is_nan() || nln <= 0.0 {
            return Ring::Unbounded;
        }
        let b = self.biquad();
        let h = |n: i64| {
            b.b0 * self.all_pole(n) + b.b1 * self.all_pole(n - 1) + b.b2 * self.all_pole(n - 2)
        };
        // The largest |h| we can find: a lower bound of the true one, so the
        // figure below is not short.
        let mut best = (0i64, h(0).abs());
        let mut n = 1.0f64;
        while n < 1e9 {
            let at = n as i64;
            let v = h(at).abs();
            if v > best.1 {
                best = (at, v);
            }
            n = (n * 1.189_207_115).max(n + 1.0);
        }
        let step = (best.0 / 64).max(1);
        let mut at = (best.0 - best.0 / 6).max(0);
        while at <= best.0 + best.0 / 5 + 4 {
            best.1 = best.1.max(h(at).abs());
            at += step;
        }
        let eps = libm::pow(10.0, f64::from(floor.get()) / 20.0);
        let target = eps * best.1;
        if target.is_nan() || target <= 0.0 {
            return Ring::Finite(Seconds::new(0.0));
        }
        let sum_b = b.b0.abs() + b.b1.abs() + b.b2.abs();
        let (r, k) = (libm::exp(-nln), self.envelope_k());
        let bound = |n: i64| sum_b * k.min((n - 1) as f64) * libm::pow(r, (n - 2) as f64);
        // The envelope falls from its peak, at about one time constant on.
        let mut lo = ((1.0 / nln) as i64 + 3).max(3);
        if bound(lo) <= target {
            return Ring::Finite(Seconds::new(lo as f64 / self.rate.get()));
        }
        let mut hi = lo;
        while bound(hi) > target {
            lo = hi;
            hi = hi.saturating_mul(2);
            if hi > (1 << 52) {
                return Ring::Unbounded;
            }
        }
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            if bound(mid) > target {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        Ring::Finite(Seconds::new(hi as f64 / self.rate.get()))
    }
}

impl Lti for SvfModel {
    fn frequency_response(&self, output: usize, freqs: &[Hz], gain: &mut [Complex64]) {
        for (g, f) in gain.iter_mut().zip(freqs) {
            *g = if output == 0 {
                self.at(2.0 * PI * f64::from(f.get()) / self.rate.get())
            } else {
                Complex64::new(f64::NAN, f64::NAN)
            };
        }
    }

    /// The slowest pole's modulus, exactly (it is the figure, and so also a
    /// bound).
    fn spectral_radius(&self) -> f64 {
        libm::exp(-self.decay_rate())
    }

    fn l1_norm(&self) -> f64 {
        section_l1(&self.biquad())
    }

    /// The peak of `|H|`, found on a grid of 1024 frequencies and a log-spaced
    /// run around the resonance (the pole's angle) and refined by golden section
    /// at the largest, then raised by a part in a million: a sufficient bound to
    /// that figure, which the contract holds against its own grid.
    fn hinf_norm(&self) -> f64 {
        let nyquist = PI;
        let mut pts: Vec<f64> = (0..=1024)
            .map(|i| nyquist * f64::from(i) / 1024.0)
            .collect();
        let [p, _] = self.poles();
        let width = self.decay_rate();
        if p.im != 0.0 {
            let w0 = p.arg().abs();
            for i in 0..80 {
                let d = width * libm::pow(10.0, (f64::from(i) - 40.0) / 8.0);
                pts.extend([w0 + d, w0 - d]);
            }
        }
        pts.retain(|w| (0.0..=nyquist).contains(w));
        pts.sort_by(f64::total_cmp);
        pts.dedup();
        let mag = |w: f64| self.at(w).norm();
        let best = pts
            .iter()
            .enumerate()
            .max_by(|a, b| mag(*a.1).total_cmp(&mag(*b.1)))
            .map_or(0, |(i, _)| i);
        let (mut a, mut b) = (
            pts[best.saturating_sub(1)],
            pts[(best + 1).min(pts.len() - 1)],
        );
        let phi = 0.618_033_988_749_894_9;
        for _ in 0..80 {
            let (x, y) = (b - phi * (b - a), a + phi * (b - a));
            if mag(x) < mag(y) {
                a = x;
            } else {
                b = y;
            }
        }
        mag(pts[best]).max(mag(0.5 * (a + b))) * (1.0 + 1e-6)
    }
}

#[cfg(test)]
mod tests;
