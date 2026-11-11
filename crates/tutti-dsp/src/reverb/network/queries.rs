//! The analytic queries of a [`Network`]: pure functions of the value.

use tutti_types::{Db, Hz, Samples, Seconds};

use crate::{Ring, System};

use super::gain::decay_seconds;
use super::{AllPass, AllPassForm, Band, Fdn, Matrix, Network, Stage};

/// How far from 1 a gain may be and still be lossless.
const LOSSLESS_TOLERANCE: f64 = 1e-6;
/// How far a matrix may be from orthogonal and still be.
const ORTHOGONAL_TOLERANCE: f64 = 1e-6;

impl Fdn {
    pub(super) fn mixing(&self) -> bool {
        !matches!(self.matrix, Matrix::Diagonal)
    }

    /// Whether the matrix has the shape its kind needs for `n` lines.
    pub(super) fn matrix_fits(&self, n: usize) -> bool {
        match &self.matrix {
            Matrix::Diagonal | Matrix::Householder => true,
            Matrix::Reflection(v) => v.len() == n,
            Matrix::Hadamard => n.is_power_of_two(),
            Matrix::Circulant(row) => row.len() == n,
            Matrix::Dense(a) => a.len() == n * n,
        }
    }

    /// The largest entry of `A Aᵀ - I`: zero for the structured kinds (orthogonal
    /// by construction), measured for a circulant or dense matrix, `None` when
    /// the matrix does not fit the line count.
    pub(super) fn orthogonality_error(&self) -> Option<f64> {
        let n = self.lines.len();
        if !self.matrix_fits(n) {
            return None;
        }
        let delta = |i: usize, j: usize| if i == j { 1.0 } else { 0.0 };
        Some(match &self.matrix {
            Matrix::Diagonal | Matrix::Householder | Matrix::Hadamard => 0.0,
            Matrix::Reflection(v) => {
                // `A Aᵀ - I = 4 (|v|² - 1) v vᵀ`.
                let s: f64 = v.iter().map(|&x| f64::from(x) * f64::from(x)).sum();
                let peak = v.iter().map(|&x| f64::from(x).abs()).fold(0.0, f64::max);
                4.0 * (s - 1.0).abs() * peak * peak
            }
            Matrix::Circulant(c) => (0..n)
                .map(|k| {
                    let r: f64 = (0..n)
                        .map(|j| f64::from(c[j]) * f64::from(c[(j + k) % n]))
                        .sum();
                    (r - delta(k, 0)).abs()
                })
                .fold(0.0, f64::max),
            Matrix::Dense(a) => (0..n)
                .flat_map(|i| (0..n).map(move |j| (i, j)))
                .map(|(i, j)| {
                    let dot: f64 = (0..n)
                        .map(|k| f64::from(a[i * n + k]) * f64::from(a[j * n + k]))
                        .sum();
                    (dot - delta(i, j)).abs()
                })
                .fold(0.0, f64::max),
        })
    }

    /// Whether `A` is orthogonal to the tolerance (the identity of a diagonal
    /// counts).
    fn orthogonal(&self) -> bool {
        self.orthogonality_error()
            .is_some_and(|e| e <= ORTHOGONAL_TOLERANCE)
    }

    /// An upper bound of the spectral norm `‖A‖₂`.
    ///
    /// Orthogonal to within `e` (every entry of `E = A Aᵀ - I` at most `e`):
    /// `‖E‖₂ ≤ ‖E‖_F ≤ n e`, so `‖A‖₂² ≤ 1 + n e` and `‖A‖₂ ≤ sqrt(1 + n e)`;
    /// exactly 1 where `e` is 0. The tolerance is `1e-6`, about a hundred times
    /// an `f32` entry's rounding, which makes the bound at most `1 + 3.2e-5`
    /// for the 64 lines of the largest network and is folded into the stability
    /// test, so a nearly-orthogonal matrix is never taken for an orthogonal one.
    /// Otherwise the Frobenius norm, sufficient and not necessary.
    pub(super) fn norm_bound(&self) -> f64 {
        let n = self.lines.len() as f64;
        // Exact for a reflection: the eigenvalue along `v` is `1 - 2 |v|²` and
        // the rest are 1, so a reflection of a unit vector to within an `f32`
        // entry's rounding is `1 + 2 (|v|² - 1)`, not the generic bound's `n` times
        // the entries' error.
        if let Matrix::Reflection(v) = &self.matrix {
            if !self.matrix_fits(self.lines.len()) {
                return libm::sqrt(n);
            }
            let s: f64 = v.iter().map(|&x| f64::from(x) * f64::from(x)).sum();
            return (1.0 - 2.0 * s).abs().max(1.0);
        }
        match self.orthogonality_error() {
            Some(e) if e <= ORTHOGONAL_TOLERANCE => libm::sqrt(1.0 + n * e),
            _ => match &self.matrix {
                Matrix::Diagonal | Matrix::Householder | Matrix::Hadamard => libm::sqrt(n),
                Matrix::Reflection(_) => libm::sqrt(n),
                Matrix::Circulant(c) => {
                    libm::sqrt(n * c.iter().map(|&x| f64::from(x) * f64::from(x)).sum::<f64>())
                }
                Matrix::Dense(a) => {
                    libm::sqrt(a.iter().map(|&x| f64::from(x) * f64::from(x)).sum())
                }
            },
        }
    }

    /// The largest gain of any line in any band (NaN or negative: infinity).
    pub(super) fn max_gain(&self) -> f64 {
        let mut max = 0.0f64;
        for line in self.lines.iter() {
            for band in Band::ALL {
                let g = f64::from(line.absorption.gain(band).get());
                if g.is_nan() || g < 0.0 {
                    return f64::INFINITY;
                }
                max = max.max(g);
            }
        }
        max
    }

    /// Whether every line can be analysed: a finite positive length (a
    /// delay-free loop is not a loop), a finite modulation depth, and a nested
    /// all-pass that is a lattice (the form this analysis models) with
    /// `|g| < 1` and a finite non-negative delay.
    pub(super) fn lines_hold(&self) -> bool {
        self.lines.iter().all(|l| {
            l.length.is_finite()
                && l.length > 0.0
                && l.modulation.depth.is_finite()
                && l.diffuser.is_none_or(|d| {
                    d.form == AllPassForm::Lattice
                        && d.gain.abs() < 1.0
                        && d.delay.is_finite()
                        && d.delay >= 0.0
                })
        })
    }

    /// The RT60 of the network's loop in `band`, in seconds: **an estimate**,
    /// exact only for a diagonal `A` (each line is its own loop and the slowest
    /// governs: the largest `-3 m / (fs log10 g)`) or when every line is set
    /// from one RT60. For a mixing `A` it is the length-weighted
    /// `-3 Σm / (fs Σ log10 g)`, a mean that can undercut the true decay when
    /// the lines differ: use [`Network::tail`] where a figure
    /// must cover the ring. `m` is the effective length.
    #[must_use]
    pub fn rt60(&self, band: Band, rate: f64) -> Seconds {
        let lines = self.lines.iter().map(|l| {
            (
                f64::from(l.effective_length()),
                libm::log10(f64::from(l.absorption.gain(band).get())),
            )
        });
        let seconds = if self.mixing() {
            let (m, l) = lines.fold((0.0, 0.0), |(m, l), (dm, dl)| (m + dm, l + dl));
            if self.lines.is_empty() {
                0.0
            } else {
                decay_seconds(m, l, rate)
            }
        } else {
            lines
                .map(|(m, l)| decay_seconds(m, l, rate))
                .fold(0.0, f64::max)
        };
        Seconds(seconds)
    }

    /// The slowest line's per-frame figures in `band`: `(log10 ρ, log10 σ,
    /// m)` where `ρ = max_i g_i^(1/m_i)` is the slowest amplitude factor per
    /// frame, `σ = max(‖A‖₂, 1)` and `m` the longest line, or `None` when a
    /// line cannot be analysed (a gain that is NaN or negative, a length that is
    /// not positive). A line with a nested all-pass is read at its peak group
    /// delay, `D (1 + |g|) / (1 - |g|)`, a conservative reading that is not part
    /// of the proof in [`rt60_bound`](Fdn::rt60_bound).
    pub(super) fn slowest(&self, band: Band) -> Option<(f64, f64, f64)> {
        let mut log_rho = f64::NEG_INFINITY;
        let mut longest = 0.0f64;
        for l in self.lines.iter() {
            let peak = l.diffuser.map_or(0.0, |d| {
                let g = f64::from(d.gain).abs();
                f64::from(d.delay) * (1.0 + g) / (1.0 - g)
            });
            let m = f64::from(l.length) + peak;
            let log_g = libm::log10(f64::from(l.absorption.gain(band).get()));
            if m.is_nan() || m <= 0.0 || log_g.is_nan() {
                return None;
            }
            log_rho = log_rho.max(log_g / m);
            longest = longest.max(m);
        }
        Some((log_rho, libm::log10(self.norm_bound().max(1.0)), longest))
    }

    /// An RT60 in `band` that never undercuts the true decay: the slowest
    /// line's, `-3 / (fs log10 (σ ρ))` with `ρ = max_i g_i^(1/m_i)` the slowest
    /// amplitude factor per frame and `σ = max(‖A‖₂, 1)`. Infinite when
    /// `σρ ≥ 1` or a line cannot be analysed. For a diagonal `A` it equals
    /// [`rt60`](Self::rt60).
    ///
    /// **Why it covers.** Take the lines as shift registers of `m_i` samples
    /// (`u` = frames since a sample entered), absorption `g_i` applied to a
    /// sample as it leaves and `A` mapping the leaving samples onto the entering
    /// ones. With `w = ρ²`, weigh each sample's energy by `w^u`:
    /// `V = Σ x² w^u`. A step shifts every sample (`u → u+1`, so `V` is
    /// multiplied by `w`), except that the samples leaving at `u = m_i - 1` are
    /// scaled by `g_i ≤ ρ^(m_i)`, mapped by `A` (norm `≤ σ`) and enter at
    /// `u = 0`: their weight becomes at most `σ² Σ g_i² y_i² ≤ σ² w · (their
    /// weight before)`. So `V(n+1) ≤ σ² w V(n)` with zero input, for any delays
    /// and any `A` of norm `≤ σ`. The weights lie in `[w^(m-1), 1]` for the
    /// longest line `m`, so the state's energy `E` satisfies
    /// `E(t) ≤ (σ²w)^t E(0) / w^(m-1)`: the state's amplitude is at most
    /// `(σρ)^t ρ^-(m-1)` times its start, and a map of the state (the output)
    /// is that times a constant. It reaches a fraction `a` after
    /// `ln(1/a) / ln(1/σρ)` frames plus a lag of `(m-1) ln(1/ρ) / ln(1/σρ)`
    /// frames, which [`Network::tail`] counts as the longest line plus
    /// `lag_extra_frames`. The figure is relative to
    /// the state's amplitude, as any floor is.
    ///
    /// [`rt60`](Self::rt60), the length-weighted mean, is not such a bound: for
    /// a mixing `A` with lines of different decay it can be shorter than the
    /// slowest line's.
    fn rt60_bound(&self, band: Band, rate: f64) -> Seconds {
        if self.lines.is_empty() {
            return Seconds(0.0);
        }
        let Some((log_rho, log_sigma, _)) = self.slowest(band) else {
            return Seconds(f64::INFINITY);
        };
        let per_frame = log_rho + log_sigma;
        Seconds(if per_frame >= 0.0 {
            f64::INFINITY
        } else if per_frame == f64::NEG_INFINITY {
            0.0
        } else {
            -3.0 / (rate * per_frame)
        })
    }

    /// The frames the lag of [`rt60_bound`](Fdn::rt60_bound) exceeds the
    /// longest line by, `(m-1) log σ / -log(σρ)`: zero where `σ = 1` (a
    /// structured matrix), a few frames for a measured-orthogonal one.
    fn lag_extra_frames(&self, band: Band) -> f64 {
        match self.slowest(band) {
            Some((log_rho, log_sigma, m)) if log_rho + log_sigma < 0.0 => {
                (m - 1.0).max(0.0) * log_sigma / -(log_rho + log_sigma)
            }
            _ => 0.0,
        }
    }

    /// Whether every gain is 1 (within a tolerance), `A` is orthogonal and the
    /// lines can be analysed: a loop that loses nothing. Marginal by
    /// construction: the tolerances allow a growth of at most
    /// `(1 + 1e-6) sqrt(1 + n e)` per trip.
    #[must_use]
    pub fn is_lossless(&self) -> bool {
        !self.lines.is_empty()
            && self.lines_hold()
            && self.orthogonal()
            && self.lines.iter().all(|l| {
                Band::ALL.iter().all(|&b| {
                    (f64::from(l.absorption.gain(b).get()) - 1.0).abs() <= LOSSLESS_TOLERANCE
                })
            })
    }

    /// Whether the loop cannot grow: every line analysable (`lines_hold`:
    /// no delay-free loop, no pole at or past the unit circle, no nested
    /// all-pass but a lattice), every gain `g` below 1 and `g ‖A‖₂ < 1` by
    /// `norm_bound` (sufficient, not necessary), or the
    /// lossless case, marginally stable.
    #[must_use]
    pub fn is_stable(&self) -> bool {
        let n = self.lines.len();
        if !self.matrix_fits(n) || !self.lines_hold() {
            return false;
        }
        let g = self.max_gain();
        if self.is_lossless() {
            return g <= 1.0 + LOSSLESS_TOLERANCE;
        }
        g < 1.0 && g * self.norm_bound() < 1.0
    }

    /// The longest effective line, in frames.
    pub(super) fn transit(&self) -> f64 {
        self.lines
            .iter()
            .map(|l| f64::from(l.effective_length()))
            .fold(0.0, f64::max)
    }
}

/// Frames an all-pass keeps ringing: its delay times the trips its geometric
/// train takes to fall to `amplitude` (infinity when `|g| >= 1`).
fn all_pass_ring(a: &AllPass, amplitude: f64) -> f64 {
    let g = f64::from(a.gain).abs();
    if g >= 1.0 {
        return f64::INFINITY;
    }
    let trips = if g == 0.0 || amplitude >= 1.0 {
        0.0
    } else {
        libm::ceil(libm::log(amplitude) / libm::log(g))
    };
    f64::from(a.delay) * (1.0 + trips)
}

impl Network {
    fn fdns(&self) -> impl Iterator<Item = &Fdn> {
        self.stages.iter().filter_map(|s| match s {
            Stage::Fdn(f) => Some(f),
            _ => None,
        })
    }

    /// The RT60 in `band`: the slowest decay of any recirculating stage (a
    /// [`Stage::Fir`] reports its summary; taps and series all-passes add
    /// none). Zero for a network that does not recirculate; infinity for a
    /// loop that never settles.
    ///
    /// **Exact** for a diagonal `A`, or when every line is set from one RT60;
    /// **an estimate** for a mixing `A` with lines of different decay (see
    /// [`Fdn::rt60`]); [`tail`](Self::tail) is the figure that covers the ring.
    #[must_use]
    pub fn rt60(&self, band: Band) -> Seconds {
        let rate = self.rate.get();
        let mut slowest = Seconds(0.0);
        for stage in self.stages.iter() {
            let r = match stage {
                Stage::Fdn(f) => f.rt60(band, rate),
                Stage::Fir(s) => s.rt60[band.index()],
                _ => Seconds(0.0),
            };
            slowest = slowest.max(r);
        }
        slowest
    }

    /// Whether the network has a recirculating stage and every one loses
    /// nothing: the freeze case.
    #[must_use]
    pub fn is_lossless(&self) -> bool {
        let mut any = false;
        for f in self.fdns() {
            if !f.is_lossless() {
                return false;
            }
            any = true;
        }
        any
    }

    /// The modes per hertz: the total delay in seconds, every line, all-pass and
    /// FIR of every channel summed.
    #[must_use]
    pub fn modal_density(&self) -> Seconds {
        let frames: f64 = self
            .stages
            .iter()
            .map(|s| match s {
                Stage::Fdn(f) => f
                    .lines
                    .iter()
                    .map(|l| f64::from(l.effective_length()))
                    .sum(),
                Stage::AllPass(a) => a.iter().map(|a| f64::from(a.delay)).sum(),
                Stage::Fir(s) => s.length.get() as f64,
                Stage::Taps(_) => 0.0,
            })
            .sum();
        Seconds(frames / self.rate.get())
    }

    /// Echoes per second `at` this long after an impulse, from the loops alone.
    /// An ordering figure, not an absolute one.
    ///
    /// A diagonal `A` never densifies: each line returns one echo per trip, so
    /// the density is the sum of `1/τ` over the lines that have come round. A
    /// mixing `A` multiplies every echo by the line count each trip, `N^k / τ̄`
    /// after `k` trips of the mean length `τ̄` (Jot's analysis), capped at one
    /// echo per frame. A FIR is one echo per frame while it lasts.
    #[must_use]
    pub fn echo_density(&self, at: Seconds) -> Hz {
        let rate = self.rate.get();
        let t = at.get().max(0.0);
        let mut total = 0.0f64;
        for stage in self.stages.iter() {
            match stage {
                Stage::Fdn(f) if !f.lines.is_empty() => {
                    total += fdn_density(f, rate, t);
                }
                Stage::Fir(s) if t * rate < s.length.get() as f64 => total += rate,
                _ => {}
            }
        }
        Hz(total as f32)
    }

    /// The shortest path from the input to the output, in frames: the
    /// pre-delay plus each stage's shortest path (a series all-pass has a
    /// direct term, zero; a line, its length; a tap, its delay; a FIR, its
    /// onset). Infinity when a stage has no path.
    #[must_use]
    pub fn min_delay(&self) -> f32 {
        let mut frames = self.pre_delay.get() as f32;
        for stage in self.stages.iter() {
            frames += match stage {
                Stage::Taps(t) => t.taps.iter().map(|t| t.delay).fold(f32::INFINITY, f32::min),
                // A form with a direct term (`-g`, or `-1`) is a path of no
                // delay; a lattice at `g = 0` is a plain delay.
                Stage::AllPass(a) => a
                    .iter()
                    .map(|a| if a.has_direct_term() { 0.0 } else { a.delay })
                    .fold(f32::INFINITY, f32::min),
                Stage::Fdn(f) => f
                    .lines
                    .iter()
                    .map(|l| l.length)
                    .fold(f32::INFINITY, f32::min),
                Stage::Fir(s) => s.onset.get() as f32,
            };
        }
        frames
    }

    /// Whether the output can depend on the input of the same frame: the
    /// shortest path is under one frame. A wet-only design has none.
    #[must_use]
    pub fn has_direct_path(&self) -> bool {
        self.min_delay() < 1.0
    }
}

impl System for Network {
    /// The processing latency: the largest of the FIR stages' (zero for the
    /// time-domain stages).
    fn latency(&self) -> Samples {
        self.stages
            .iter()
            .filter_map(|s| match s {
                Stage::Fir(s) => Some(s.latency),
                _ => None,
            })
            .max()
            .unwrap_or(Samples::ZERO)
    }

    /// How long the wet signal rings after the input stops, until it is `floor`
    /// below what it carried: the pre-delay, plus each stage's transit (the
    /// longest line, the longest tap, an all-pass's delay times the trips of its
    /// train, a FIR's length), plus the slowest band's
    /// `Fdn::rt60_bound` times `|floor| / 60` (with the
    /// lag the bound's proof adds). It uses the bound,
    /// not the estimate, so it never undercuts a mixing network whose lines
    /// differ.
    ///
    /// [`Ring::Unbounded`] when a loop never settles (lossless, or a gain at 1
    /// or above), or when `floor` is NaN. A positive `floor` is read as 0 dB.
    fn tail(&self, floor: Db) -> Ring {
        if floor.get().is_nan() {
            return Ring::Unbounded;
        }
        let depth = f64::from((-floor.get()).max(0.0));
        let amplitude = libm::exp10(-depth / 20.0);
        let mut decay = 0.0f64;
        let mut frames = self.pre_delay.get() as f64;
        for stage in self.stages.iter() {
            match stage {
                Stage::Taps(t) => {
                    frames += t
                        .taps
                        .iter()
                        .map(|t| f64::from(t.delay))
                        .fold(0.0, f64::max);
                }
                Stage::AllPass(a) => {
                    frames += a
                        .iter()
                        .map(|a| all_pass_ring(a, amplitude))
                        .fold(0.0, f64::max);
                }
                Stage::Fdn(f) => {
                    frames += f.transit();
                    if f.is_lossless() {
                        return Ring::Unbounded;
                    }
                    for band in Band::ALL {
                        let rate = self.rate.get();
                        let seconds = f.rt60_bound(band, rate).get() * depth / 60.0
                            + f.lag_extra_frames(band) / rate;
                        decay = decay.max(seconds);
                    }
                }
                Stage::Fir(s) => frames += s.length.get() as f64,
            }
        }
        if !frames.is_finite() || !decay.is_finite() {
            return Ring::Unbounded;
        }
        Ring::Finite(Seconds(frames / self.rate.get() + decay))
    }
}

fn fdn_density(f: &Fdn, rate: f64, t: f64) -> f64 {
    let n = f.lines.len() as f64;
    let taus = f
        .lines
        .iter()
        .map(|l| f64::from(l.effective_length()) / rate);
    if f.mixing() {
        let mean = taus.sum::<f64>() / n;
        let first = f
            .lines
            .iter()
            .map(|l| f64::from(l.effective_length()) / rate)
            .fold(f64::INFINITY, f64::min);
        if t < first || mean <= 0.0 {
            return 0.0;
        }
        let trips = libm::floor(t / mean).max(1.0);
        (libm::pow(n, trips) / mean).min(rate)
    } else {
        taus.filter(|&tau| tau <= t && tau > 0.0)
            .map(|tau| 1.0 / tau)
            .sum()
    }
}

#[cfg(test)]
mod tests;
