//! A [`Network`] as a linear time-invariant system: [`Lti`] (design 024
//! step 1b).
//!
//! The response is composed from the stages, never from a pole list: a series of
//! stages carries a vector of channels, and a feedback delay network is solved
//! over its own lines (`y = C D (I - A G D)^-1 b`, the structure of
//! [`FdnKernel`](crate::FdnKernel)). The bounds are sufficient ones and say so.
//!
//! **What the model leaves out, on purpose.** Modulation moves a line around its
//! centre length, and the model reads the centre. A [`Stage::Fir`] holds a
//! summary and not the response, so it has no frequency response (NaN), no
//! finite `l1_norm` and radius 0. [`FdnKernel`](crate::FdnKernel) rescales a set
//! of band gains that would exceed 1 anywhere to peak at 0.9999; the model does not,
//! and [`Absorption::peak`] is what shows a design never asks for it.

use tutti_types::{Hz, SampleRate};

use super::{
    Absorption, AllPass, AllPassForm, Band, Fdn, Line, MAX_CHANNELS, Matrix, Network, Stage,
};
use crate::{Complex64, Lti};

const ZERO: Complex64 = Complex64::new(0.0, 0.0);
const ONE: Complex64 = Complex64::new(1.0, 0.0);

/// The points of the grid `Absorption::peak` reads the response at.
const PEAK_POINTS: u32 = 192;
/// The radii `l1_norm` tries for an FDN, as `1 - r` on a log grid.
const RADII: u32 = 48;

/// `e^(-j w d)`: a delay of `d` frames at angular frequency `w`.
fn delay(w: f64, d: f64) -> Complex64 {
    Complex64::from_polar(1.0, -w * d)
}

impl Absorption {
    /// The response `G(e^jw)` of one trip through the loop at angular frequency
    /// `w`. NaN for a one-pole that does not settle.
    fn response(&self, w: f64, rate: f64) -> Complex64 {
        match *self {
            Absorption::Gain(g) => Complex64::new(f64::from(g.get()), 0.0),
            Absorption::OnePole { gain, damp } => {
                let d = f64::from(damp);
                if !(d.is_finite() && d.abs() < 1.0) {
                    return Complex64::new(f64::NAN, f64::NAN);
                }
                f64::from(gain.get()) * (1.0 - d) / (ONE - delay(w, 1.0) * d)
            }
            Absorption::Bands {
                low,
                mid,
                high,
                crossover,
            } => {
                let (low, mid, high) = (
                    f64::from(low.get()),
                    f64::from(mid.get()),
                    f64::from(high.get()),
                );
                // `L = ((1 - p) / (1 - p z^-1))^2`, as the kernel's two cascaded
                // one-poles; `L1` at the lower crossover, `L2` at the upper.
                let l = |fc: Hz| {
                    let p = libm::exp(-std::f64::consts::TAU * f64::from(fc.get()) / rate);
                    let one = (1.0 - p) / (ONE - delay(w, 1.0) * p);
                    one * one
                };
                high + (mid - high) * l(crossover[1]) + (low - mid) * l(crossover[0])
            }
        }
    }

    /// The largest `|G|` of the absorption over a grid of frequencies from DC to
    /// Nyquist (dense toward DC, where the bands change): what
    /// [`FdnKernel`](crate::FdnKernel) rescales when it exceeds 1. A design whose
    /// absorptions peak at 1 or under is one the kernel runs as the model says.
    #[must_use]
    pub fn peak(&self, rate: SampleRate) -> f64 {
        (0..=PEAK_POINTS)
            .map(|k| {
                let w = std::f64::consts::PI * (f64::from(k) / f64::from(PEAK_POINTS)).powi(2);
                self.response(w, rate.get()).norm()
            })
            .fold(0.0, f64::max)
    }

    /// A bound on the largest `|G(e^jw)|`: the gain of a [`Gain`](Self::Gain), the
    /// exact peak of a one-pole (at DC or at Nyquist), and for bands the largest
    /// of a grid of 4096 frequencies plus what the response can move between two
    /// of them (its slope is at most `2 p / (1 - p)` times each term's weight, for
    /// each pole `p`). Tighter than the `majorant` at `r = 1` when the bands
    /// rise with frequency, where the sum of the magnitudes of the three weights
    /// exceeds the peak.
    fn sup(&self, rate: f64) -> f64 {
        let Absorption::Bands {
            low,
            mid,
            high,
            crossover,
        } = *self
        else {
            return self.majorant(1.0, rate);
        };
        const POINTS: usize = 4096;
        let (low, mid, high) = (
            f64::from(low.get()),
            f64::from(mid.get()),
            f64::from(high.get()),
        );
        let pole = |fc: Hz| libm::exp(-std::f64::consts::TAU * f64::from(fc.get()) / rate);
        let (p1, p2) = (pole(crossover[0]), pole(crossover[1]));
        if [low, mid, high].iter().any(|g| !g.is_finite() || *g < 0.0)
            || ![p1, p2]
                .iter()
                .all(|p| p.is_finite() && *p > 0.0 && *p < 1.0)
        {
            return f64::INFINITY;
        }
        let slope =
            (mid - high).abs() * 2.0 * p2 / (1.0 - p2) + (low - mid).abs() * 2.0 * p1 / (1.0 - p1);
        let peak = (0..=POINTS)
            .map(|k| {
                self.response(std::f64::consts::PI * k as f64 / POINTS as f64, rate)
                    .norm()
            })
            .fold(0.0, f64::max);
        peak + slope * std::f64::consts::PI / POINTS as f64 / 2.0
    }

    /// `sum |g_k| r^-k` over the impulse response of one trip, for `r` in
    /// `(0, 1]`: the l1 norm at `r = 1`, and a bound on `|G|` on the circle
    /// `|z| = r` (a delay of `D` frames there has magnitude `r^-D`). Infinite when
    /// a pole is not inside the circle, and for a gain that is not a loop gain.
    fn majorant(&self, r: f64, rate: f64) -> f64 {
        match *self {
            Absorption::Gain(g) => nonnegative(f64::from(g.get())),
            Absorption::OnePole { gain, damp } => {
                let d = f64::from(damp);
                let g = f64::from(gain.get());
                if !(d.is_finite() && g.is_finite() && g >= 0.0 && d.abs() < r) {
                    return f64::INFINITY;
                }
                // The impulse response is `g (1 - d) d^k`.
                g * (1.0 - d).abs() / (1.0 - d.abs() / r)
            }
            Absorption::Bands {
                low,
                mid,
                high,
                crossover,
            } => {
                let (low, mid, high) = (
                    f64::from(low.get()),
                    f64::from(mid.get()),
                    f64::from(high.get()),
                );
                if [low, mid, high].iter().any(|g| !g.is_finite() || *g < 0.0) {
                    return f64::INFINITY;
                }
                // A cascade of two identical one-poles has the nonnegative impulse
                // response of `((1 - p) / (1 - p z^-1))^2`; its sum at radius `r` is
                // `((1 - p) / (1 - p / r))^2` while `p < r`. The three terms'
                // coefficients are weighted by their gains' magnitudes.
                let sum = |fc: Hz| {
                    let p = libm::exp(-std::f64::consts::TAU * f64::from(fc.get()) / rate);
                    if p < r {
                        let x = (1.0 - p) / (1.0 - p / r);
                        x * x
                    } else {
                        f64::INFINITY
                    }
                };
                high.abs()
                    + (mid - high).abs() * sum(crossover[1])
                    + (low - mid).abs() * sum(crossover[0])
            }
        }
    }
}

fn nonnegative(g: f64) -> f64 {
    if g.is_finite() && g >= 0.0 {
        g
    } else {
        f64::INFINITY
    }
}

impl AllPass {
    /// `H(e^jw)`: the lattice `(z^-D - g) / (1 - g z^-D)` or Freeverb's
    /// `((1 + g) z^-D - 1) / (1 - g z^-D)`.
    fn response(&self, w: f64) -> Complex64 {
        let (g, zd) = (f64::from(self.gain), delay(w, f64::from(self.delay)));
        let den = ONE - zd * g;
        match self.form {
            AllPassForm::Lattice => (zd - g) / den,
            AllPassForm::Freeverb => (zd * (1.0 + g) - ONE) / den,
        }
    }

    /// `sum |h_k| r^-k` of the impulse response, at radius `r` in `(0, 1]` (the
    /// l1 norm at 1). The lattice has `-g` at 0 and `(1 - g^2) g^(k-1)` at `kD`;
    /// Freeverb's has `-1` at 0 and `(1 + g) g^(k-1) - g^k = g^(k-1)` at `kD`.
    /// `1 + 2|g|` and `1 + 1/(1 - |g|)` at `r = 1`. Infinity when `|g| r^-D >= 1`.
    fn majorant(&self, r: f64) -> f64 {
        let g = f64::from(self.gain).abs();
        let d = f64::from(self.delay);
        if !(g.is_finite() && d.is_finite() && d >= 0.0 && g < 1.0) {
            return f64::INFINITY;
        }
        let back = libm::pow(r, -d);
        let ratio = g * back;
        if ratio >= 1.0 {
            return f64::INFINITY;
        }
        match self.form {
            AllPassForm::Lattice => g + (1.0 - g * g) * back / (1.0 - ratio),
            AllPassForm::Freeverb => 1.0 + back / (1.0 - ratio),
        }
    }

    /// The largest `|H(e^jw)|`: 1 for the lattice, and for Freeverb's the larger
    /// of its two extremes (`z^-D = 1` and `-1`; `|H|` is monotone in `cos(wD)`).
    fn peak(&self) -> f64 {
        let g = f64::from(self.gain);
        if !(g.is_finite() && self.delay.is_finite() && self.delay >= 0.0 && g.abs() < 1.0) {
            return f64::INFINITY;
        }
        match self.form {
            AllPassForm::Lattice => 1.0,
            AllPassForm::Freeverb => (g / (1.0 - g)).abs().max((2.0 + g).abs() / (1.0 + g).abs()),
        }
    }

    /// The pole radius `|g|^(1/D)`: an all-pass of no delay is memoryless.
    fn radius(&self) -> f64 {
        let g = f64::from(self.gain).abs();
        let d = f64::from(self.delay);
        if !(g.is_finite() && d.is_finite() && d >= 0.0 && g < 1.0) {
            return f64::INFINITY;
        }
        if g == 0.0 {
            0.0
        } else {
            libm::pow(g, 1.0 / d.max(1.0))
        }
    }
}

impl Line {
    /// One trip's response `G(e^jw) AP(e^jw) z^-m`: the diagonal of the loop `T`.
    fn trip(&self, w: f64, rate: f64) -> Complex64 {
        let mut t = self.absorption.response(w, rate) * delay(w, f64::from(self.length));
        if let Some(d) = self.diffuser {
            t *= d.response(w);
        }
        t
    }
}

impl Matrix {
    /// The entry `A[i][j]` of the `n` by `n` matrix, or `None` when the shape does
    /// not fit `n` lines.
    fn entry(&self, n: usize, i: usize, j: usize) -> Option<f64> {
        Some(match self {
            Matrix::Diagonal => f64::from(u8::from(i == j)),
            Matrix::Householder => f64::from(u8::from(i == j)) - 2.0 / n as f64,
            Matrix::Reflection(v) => {
                if v.len() != n {
                    return None;
                }
                f64::from(u8::from(i == j)) - 2.0 * f64::from(v[i]) * f64::from(v[j])
            }
            Matrix::Hadamard => {
                if !n.is_power_of_two() {
                    return None;
                }
                let sign = if (i & j).count_ones().is_multiple_of(2) {
                    1.0
                } else {
                    -1.0
                };
                sign / libm::sqrt(n as f64)
            }
            Matrix::Circulant(c) => {
                if c.len() != n {
                    return None;
                }
                f64::from(c[(j + n - i) % n])
            }
            Matrix::Dense(a) => {
                if a.len() != n * n {
                    return None;
                }
                f64::from(a[i * n + j])
            }
        })
    }
}

/// Solves `M x = b` for complex `M` (row-major, `n` by `n`) by Gaussian
/// elimination with partial pivoting. A singular matrix gives infinities or NaN.
fn solve(n: usize, m: &mut [Complex64], b: &mut [Complex64]) {
    for col in 0..n {
        let pivot = (col..n)
            .max_by(|&a, &c| m[a * n + col].norm().total_cmp(&m[c * n + col].norm()))
            .unwrap_or(col);
        if pivot != col {
            for k in 0..n {
                m.swap(col * n + k, pivot * n + k);
            }
            b.swap(col, pivot);
        }
        let p = m[col * n + col];
        for row in col + 1..n {
            let f = m[row * n + col] / p;
            if f == ZERO {
                continue;
            }
            for k in col..n {
                let sub = f * m[col * n + k];
                m[row * n + k] -= sub;
            }
            let sub = f * b[col];
            b[row] -= sub;
        }
    }
    for row in (0..n).rev() {
        let mut v = b[row];
        for k in row + 1..n {
            v -= m[row * n + k] * b[k];
        }
        b[row] = v / m[row * n + row];
    }
}

impl Fdn {
    /// The state `s = (I - A T)^-1 b` for the loop `T = diag(t)`, one entry a line,
    /// and the whole row `C D s` one row at a time into `out`: `out[r]` is
    /// the transfer from the mono input to output row `r`. `None` when the matrix
    /// does not fit the lines.
    fn solve_state(&self, t: &[Complex64], s: &mut [Complex64]) -> Option<()> {
        let n = self.lines.len();
        if !self.matrix_fits(n) {
            return None;
        }
        for (si, b) in s.iter_mut().zip(self.input.iter()) {
            *si = Complex64::new(f64::from(*b), 0.0);
        }
        // `A = I - 2 u u^T` for a unit `u` (a rank one change of the identity):
        // `I - A T = (I - T) + 2 u (T u)^T`, solved by Sherman and Morrison in O(n).
        let rank_one = |u: &dyn Fn(usize) -> f64, s: &mut [Complex64]| {
            let inv: Vec<Complex64> = t.iter().map(|ti| ONE / (ONE - ti)).collect();
            // `(D0 + p q^T)^-1 b = D0^-1 b - D0^-1 p (q^T D0^-1 b) / (1 + q^T D0^-1 p)`
            // with `D0 = I - T`, `p = 2u`, `q = T u`.
            let (mut qb, mut qp) = (ZERO, ZERO);
            for i in 0..n {
                let q = t[i] * u(i);
                qb += q * inv[i] * s[i];
                qp += q * inv[i] * 2.0 * u(i);
            }
            let k = qb / (ONE + qp);
            for i in 0..n {
                s[i] = inv[i] * (s[i] - 2.0 * u(i) * k);
            }
        };
        match &self.matrix {
            Matrix::Diagonal => {
                for (si, ti) in s.iter_mut().zip(t) {
                    *si /= ONE - ti;
                }
            }
            Matrix::Householder => {
                let u = 1.0 / libm::sqrt(n as f64);
                rank_one(&|_| u, s);
            }
            Matrix::Reflection(v) => rank_one(&|i| f64::from(v[i]), s),
            other => {
                let mut m = vec![ZERO; n * n];
                for i in 0..n {
                    for j in 0..n {
                        let a = other.entry(n, i, j)?;
                        m[i * n + j] = if i == j { ONE } else { ZERO } - t[j] * a;
                    }
                }
                solve(n, &mut m, s);
            }
        }
        Some(())
    }
}

impl Network {
    /// The channels after the last stage: the width the signal has when it leaves
    /// (a stage with no recirculation or taps leaves it as it found it).
    fn width_after(&self) -> usize {
        let mut width = 1;
        for stage in self.stages.iter() {
            width = match stage {
                Stage::Taps(t) => t
                    .taps
                    .iter()
                    .map(|t| usize::from(t.channel) + 1)
                    .max()
                    .unwrap_or(1),
                Stage::AllPass(a) if !a.is_empty() => a.len(),
                Stage::AllPass(_) => width,
                Stage::Fdn(f) => f.output.rows.len().max(1),
                Stage::Fir(_) => width,
            };
        }
        width.min(MAX_CHANNELS)
    }

    /// The signal's channels at angular frequency `w`, one entry each, as the
    /// stages leave them: pre-delay, stages in series, then width and wet. `None`
    /// is a network whose response cannot be given.
    fn channels_at(&self, w: f64) -> [Complex64; MAX_CHANNELS] {
        let rate = self.rate.get();
        let mut v = [ZERO; MAX_CHANNELS];
        v[0] = delay(w, self.pre_delay.get() as f64);
        let mut width = 1usize;
        let nan = Complex64::new(f64::NAN, f64::NAN);
        // A stage that takes one signal takes the mean of the channels it is
        // handed (the kernel's mono sum of its input).
        let mean = |v: &[Complex64; MAX_CHANNELS], width: usize| -> Complex64 {
            v[..width].iter().sum::<Complex64>() / width as f64
        };
        for stage in self.stages.iter() {
            let mut out = [ZERO; MAX_CHANNELS];
            match stage {
                Stage::Taps(t) => {
                    let x = mean(&v, width);
                    let mut next = 1;
                    for tap in t.taps.iter() {
                        let c = usize::from(tap.channel).min(MAX_CHANNELS - 1);
                        out[c] += x * f64::from(tap.gain) * delay(w, f64::from(tap.delay));
                        next = next.max(c + 1);
                    }
                    width = next;
                }
                Stage::AllPass(a) => {
                    for (k, ap) in a.iter().enumerate() {
                        let x = if width == 1 {
                            v[0]
                        } else if k < width {
                            v[k]
                        } else {
                            ZERO
                        };
                        out[k] = x * ap.response(w);
                    }
                    if !a.is_empty() {
                        width = a.len();
                    } else {
                        out = v;
                    }
                }
                Stage::Fdn(f) => {
                    let n = f.lines.len();
                    let x = mean(&v, width);
                    let t: Vec<Complex64> = f.lines.iter().map(|l| l.trip(w, rate)).collect();
                    let mut s = vec![ZERO; n];
                    if f.solve_state(&t, &mut s).is_none() {
                        out = [nan; MAX_CHANNELS];
                    } else {
                        // `C D s`: the line outputs are the delayed states.
                        let d: Vec<Complex64> = f
                            .lines
                            .iter()
                            .zip(&s)
                            .map(|(l, s)| *s * delay(w, f64::from(l.length)))
                            .collect();
                        for (r, row) in f.output.rows.iter().enumerate() {
                            out[r] = x * row
                                .iter()
                                .zip(&d)
                                .map(|(c, d)| *d * f64::from(*c))
                                .sum::<Complex64>();
                        }
                    }
                    width = f.output.rows.len().max(1);
                }
                Stage::Fir(_) => out = [nan; MAX_CHANNELS],
            }
            v = out;
        }
        // Width and wet, as the kernel's mid and side: `(M + wS, M - wS) * wet`.
        let wet = f64::from(self.wet.to_amplitude().get());
        if width >= 2 {
            let (m, s) = ((v[0] + v[1]) * 0.5, (v[0] - v[1]) * 0.5);
            let k = f64::from(self.width.get());
            v[0] = (m + s * k) * wet;
            v[1] = (m - s * k) * wet;
            for c in &mut v[2..width] {
                *c *= wet;
            }
        } else {
            v[0] *= wet;
        }
        v
    }

    /// The l1 norm (`at = Some(r)` is the radius, `1` for the l1 norm and
    /// `None` for the `hinf` figure) of the stages in series.
    fn series_gain(&self, norm: Norm) -> f64 {
        let rate = self.rate.get();
        let mut gain = 1.0f64;
        for stage in self.stages.iter() {
            gain *= match stage {
                Stage::Taps(t) => {
                    let mut per = [0.0f64; MAX_CHANNELS];
                    for tap in t.taps.iter() {
                        let (g, d) = (f64::from(tap.gain), f64::from(tap.delay));
                        if !(g.is_finite() && d.is_finite() && d >= 0.0) {
                            return f64::INFINITY;
                        }
                        per[usize::from(tap.channel).min(MAX_CHANNELS - 1)] += g.abs();
                    }
                    per.iter().copied().fold(0.0, f64::max)
                }
                Stage::AllPass(a) => a
                    .iter()
                    .map(|ap| match norm {
                        Norm::L1 => ap.majorant(1.0),
                        Norm::Hinf => ap.peak(),
                    })
                    .fold(0.0, f64::max),
                Stage::Fdn(f) => match norm {
                    Norm::L1 => f.l1_gain(rate),
                    Norm::Hinf => f.resolvent_gain(1.0, rate),
                },
                Stage::Fir(_) => f64::INFINITY,
            };
        }
        let wet = f64::from(self.wet.to_amplitude().get());
        let width = if self.width_after() >= 2 {
            f64::from(self.width.get()).abs().max(1.0)
        } else {
            1.0
        };
        let all = gain * wet * width;
        if all.is_nan() { f64::INFINITY } else { all }
    }
}

/// Which norm a series of stages is bounded in.
#[derive(Clone, Copy)]
enum Norm {
    L1,
    Hinf,
}

impl Fdn {
    /// The Euclidean norm of the largest output row, and of the input gains.
    fn row_norms(&self) -> (f64, f64) {
        let two =
            |g: &[f32]| libm::sqrt(g.iter().map(|&x| f64::from(x) * f64::from(x)).sum::<f64>());
        let c = self.output.rows.iter().map(|r| two(r)).fold(0.0, f64::max);
        (c, two(&self.input))
    }

    /// The factor one trip through line `i` multiplies a signal by on the circle
    /// `|z| = r`: the absorption's and the nested all-pass's majorants, and the
    /// delay's `r^-m`.
    fn trip_gain(&self, line: &Line, r: f64, rate: f64) -> f64 {
        let mut g = if r == 1.0 {
            line.absorption.sup(rate)
        } else {
            line.absorption.majorant(r, rate)
        };
        if let Some(d) = line.diffuser {
            // On the unit circle an all-pass has modulus 1; inside it, the
            // majorant of its impulse response bounds it.
            g *= if r == 1.0 { d.peak() } else { d.majorant(r) };
        }
        g * libm::pow(r, -f64::from(line.length + line.modulation.depth.abs()))
    }

    /// `||C|| r^-m ||B|| / (1 - q)` with `q = ||A|| max_i |G_i| r^-m_i`: a bound on
    /// `|H(z)|` for `|z| = r` of the transfer to any one output. It is the resolvent
    /// bound `||(I - X)^-1|| <= 1 / (1 - ||X||)` for `X = A G D`, `||D|| = r^-m`
    /// of the longest line, and the unit modulus of a delay at `r = 1`. Infinity
    /// where `q >= 1`, and for a network the analysis does not accept.
    fn resolvent_gain(&self, r: f64, rate: f64) -> f64 {
        let n = self.lines.len();
        if n == 0 {
            return 0.0;
        }
        if !self.matrix_fits(n) || !self.lines_hold() {
            return f64::INFINITY;
        }
        let q = self.norm_bound()
            * self
                .lines
                .iter()
                .map(|l| self.trip_gain(l, r, rate))
                .fold(0.0, f64::max);
        if q.is_nan() || q >= 1.0 {
            return f64::INFINITY;
        }
        let longest = self
            .lines
            .iter()
            .map(|l| f64::from(l.length + l.modulation.depth.abs()))
            .fold(0.0, f64::max);
        let (c, b) = self.row_norms();
        c * libm::pow(r, -longest) * b / (1.0 - q)
    }

    /// A bound on the l1 norm of the impulse response to any one output.
    ///
    /// **Diagonal `A`**: the lines do not couple, each is `b_i z^-m_i / (1 - G_i
    /// z^-m_i)`, whose impulse response is `b_i` times a sum of the `k`-fold
    /// self-convolutions of `g_i`, so its l1 norm is at most `|b_i| / (1 - a_i)`
    /// with `a_i = ||g_i||_1` (the l1 norm is submultiplicative, and a delay has
    /// norm 1); a row of outputs sums them: `sum_i |c_ri| |b_i| / (1 - a_i)`.
    ///
    /// **Any `A`**: for `0 < r < 1`, Cauchy and Schwarz give
    /// `sum |h_n| <= (sum r^(2n))^(1/2) (sum |h_n|^2 r^(-2n))^(1/2)`, and the
    /// second factor is the `H2` norm of `H` on the circle `|z| = r` (Parseval), at most the
    /// largest `|H|` there, which [`resolvent_gain`](Self::resolvent_gain) bounds.
    /// So `||h||_1 <= resolvent_gain(r) / sqrt(1 - r^2)` for every `r` the gain is
    /// finite at, and the smallest over a log grid of `1 - r` is returned. The
    /// bound is loose (the true figure of a dense network is far under it) and is
    /// finite while `||A|| max_i |G_i| r^(-m_i)` can be brought under 1, that is
    /// while the slowest line's loop gain per frame is under `1 / ||A||`.
    fn l1_gain(&self, rate: f64) -> f64 {
        let n = self.lines.len();
        if n == 0 {
            return 0.0;
        }
        if !self.matrix_fits(n) || !self.lines_hold() {
            return f64::INFINITY;
        }
        let mut best = (1..=RADII)
            .map(|k| {
                let eps = libm::pow(10.0, -f64::from(k) / 8.0);
                let r = 1.0 - eps;
                self.resolvent_gain(r, rate) / libm::sqrt(1.0 - r * r)
            })
            .filter(|g| !g.is_nan())
            .fold(f64::INFINITY, f64::min);
        if matches!(self.matrix, Matrix::Diagonal) {
            let a: Vec<f64> = self
                .lines
                .iter()
                .map(|l| {
                    let ap = l.diffuser.map_or(1.0, |d| d.majorant(1.0));
                    l.absorption.majorant(1.0, rate) * ap
                })
                .collect();
            let exact = self
                .output
                .rows
                .iter()
                .map(|row| {
                    row.iter()
                        .zip(self.input.iter())
                        .zip(&a)
                        .map(|((c, b), a)| {
                            let w = f64::from(*c).abs() * f64::from(*b).abs();
                            if w == 0.0 { 0.0 } else { w / (1.0 - a) }
                        })
                        .map(|x| if x < 0.0 { f64::INFINITY } else { x })
                        .sum::<f64>()
                })
                .fold(0.0, f64::max);
            best = best.min(exact);
        }
        best
    }

    /// The loop's spectral radius bound in `band`-wise form: the largest
    /// `10^(log rho + log sigma)` over the bands, 0 for an empty one and
    /// infinity for one the analysis does not accept. A lossless loop is at 1.
    fn radius(&self) -> f64 {
        let n = self.lines.len();
        if n == 0 {
            return 0.0;
        }
        if !self.matrix_fits(n) || !self.lines_hold() {
            return f64::INFINITY;
        }
        if self.is_lossless() {
            return 1.0;
        }
        let mut worst = 0.0f64;
        for band in Band::ALL {
            let Some((log_rho, log_sigma, _)) = self.slowest(band) else {
                return f64::INFINITY;
            };
            worst = worst.max(libm::pow(10.0, log_rho + log_sigma));
        }
        worst
    }
}

impl Lti for Network {
    fn inputs(&self) -> usize {
        1
    }

    /// The channels after the last stage.
    fn outputs(&self) -> usize {
        self.width_after()
    }

    /// `H(e^jw)` from the input to `output`: the pre-delay, the stages in series
    /// (a [`Taps`](super::Taps) sum of delays, a series all-pass per channel and an
    /// [`Fdn`] solved over its own lines: a diagonal `A` directly, a Householder or
    /// reflection by a rank-one update, any other matrix by Gaussian elimination),
    /// then width and wet. A stage that takes one signal is handed the mean of
    /// its input channels. NaN for an `output` the network does not have, for a
    /// [`Stage::Fir`] (a summary has no response), and for a network that does not
    /// fit its matrix. `freqs` are `f32` hertz, so a response read near a pole
    /// is only as sharp as the grid's own rounding.
    fn frequency_response(&self, output: usize, freqs: &[Hz], gain: &mut [Complex64]) {
        let rate = self.rate.get();
        let width = self.width_after();
        for (g, f) in gain.iter_mut().zip(freqs) {
            let w = std::f64::consts::TAU * f64::from(f.get()) / rate;
            *g = if output < width {
                self.channels_at(w)[output]
            } else {
                Complex64::new(f64::NAN, f64::NAN)
            };
        }
    }

    /// The slowest decay per frame over the stages: for a loop, `sigma rho` of the
    /// slowest band (`Fdn::rt60_bound`'s own figure, `10^(log rho + log
    /// sigma)` per frame); a series all-pass's `|g|^(1/D)`; 0 for taps and a FIR; 1
    /// for a lossless loop; infinity for what the analysis does not accept.
    fn spectral_radius(&self) -> f64 {
        self.stages
            .iter()
            .map(|s| match s {
                Stage::Fdn(f) => f.radius(),
                Stage::AllPass(a) => a.iter().map(AllPass::radius).fold(0.0, f64::max),
                Stage::Taps(t) => {
                    if t.taps
                        .iter()
                        .all(|t| t.gain.is_finite() && t.delay.is_finite())
                    {
                        0.0
                    } else {
                        f64::INFINITY
                    }
                }
                Stage::Fir(_) => 0.0,
            })
            .fold(0.0, f64::max)
    }

    /// A bound on `||h||_1`: the stages' bounds multiplied (the l1 norm is
    /// submultiplicative), by the width and the wet gain. Per stage: taps `sum
    /// |gain|`; a lattice all-pass `1 + 2|g|` and Freeverb's `1 + 1 / (1 - |g|)`
    /// (their impulse responses summed); an [`Fdn`] as in
    /// [`Fdn::l1_gain`](Fdn) (the exact figure for a diagonal `A`, a resolvent
    /// bound on shrunk circles for any other). Infinity for a
    /// [`Stage::Fir`], whose response is not in the value, and wherever a loop's
    /// gain is not under 1.
    ///
    /// The width stage `(M + wS, M - wS)` has l1 gain `max(1, w)`.
    fn l1_norm(&self) -> f64 {
        self.series_gain(Norm::L1)
    }

    /// A bound on `||H||_inf`: the resolvent bound `||C|| ||B|| / (1 - sigma
    /// g_inf)` for each loop (`g_inf` the largest `|G|`, `sigma = ||A||`), the
    /// peak of each all-pass (1 for a lattice), and the taps' `sum |gain|`;
    /// never above [`l1_norm`](Self::l1_norm), which is also a bound.
    fn hinf_norm(&self) -> f64 {
        self.series_gain(Norm::Hinf).min(self.l1_norm())
    }

    /// Whether no loop can grow. Every recirculating stage must be stable
    /// ([`Fdn::is_stable`]), every series all-pass must have `|g| < 1`, and the
    /// figures must be finite.
    ///
    /// This is not `spectral_radius() < 1`: a frozen loop has radius 1 and is
    /// accepted (it is marginal, not growing), and the hostile figures
    /// the design clamps (a delay-free loop, a pole at the unit circle, a nested
    /// all-pass that is not a lattice) are refused here by name.
    fn is_stable(&self) -> bool {
        self.stages.iter().all(|s| match s {
            Stage::Fdn(f) => f.is_stable(),
            Stage::AllPass(a) => a
                .iter()
                .all(|a| a.gain.abs() < 1.0 && a.delay.is_finite() && a.delay >= 0.0),
            Stage::Taps(t) => t
                .taps
                .iter()
                .all(|t| t.gain.is_finite() && t.delay.is_finite() && t.delay >= 0.0),
            Stage::Fir(s) => s.rt60.iter().all(|r| !r.get().is_nan()),
        })
    }
}

#[cfg(test)]
mod tests;
