//! The analytic queries of a [`Network`]: pure functions of the value.

use tutti_types::{Db, Hz, Samples, Seconds};

use super::gain::decay_seconds;
use super::{AllPass, Band, Fdn, Matrix, Network, Stage};

/// How far from 1 a gain may be and still be lossless.
const LOSSLESS_TOLERANCE: f64 = 1e-6;
/// How far a matrix may be from orthogonal and still be.
const ORTHOGONAL_TOLERANCE: f64 = 1e-5;

/// How long a network rings: [`Network::tail`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Ring {
    /// It falls to the floor in this long.
    Finite(Seconds),
    /// It never does: a lossless loop, or one that grows.
    Unbounded,
}

impl Ring {
    /// Whether this ring lasts at least as long as `other`.
    #[must_use]
    pub fn covers(self, other: Ring) -> bool {
        match (self, other) {
            (Ring::Unbounded, _) => true,
            (Ring::Finite(_), Ring::Unbounded) => false,
            (Ring::Finite(a), Ring::Finite(b)) => a.get() >= b.get(),
        }
    }

    /// The finite figure, or `None` for [`Unbounded`](Self::Unbounded).
    #[must_use]
    pub fn seconds(self) -> Option<Seconds> {
        match self {
            Ring::Finite(s) => Some(s),
            Ring::Unbounded => None,
        }
    }
}

impl Fdn {
    fn mixing(&self) -> bool {
        !matches!(self.matrix, Matrix::Diagonal)
    }

    /// Whether the matrix has the shape its kind needs for `n` lines.
    fn matrix_fits(&self, n: usize) -> bool {
        match &self.matrix {
            Matrix::Diagonal | Matrix::Householder => true,
            Matrix::Hadamard => n.is_power_of_two(),
            Matrix::Circulant(row) => row.len() == n,
            Matrix::Dense(a) => a.len() == n * n,
        }
    }

    /// Whether `A` is orthogonal (the identity of a diagonal counts).
    fn orthogonal(&self) -> bool {
        let n = self.lines.len();
        if !self.matrix_fits(n) {
            return false;
        }
        match &self.matrix {
            Matrix::Diagonal | Matrix::Householder | Matrix::Hadamard => true,
            Matrix::Circulant(c) => (0..n).all(|k| {
                let r: f64 = (0..n)
                    .map(|j| f64::from(c[j]) * f64::from(c[(j + k) % n]))
                    .sum();
                (r - if k == 0 { 1.0 } else { 0.0 }).abs() <= ORTHOGONAL_TOLERANCE
            }),
            Matrix::Dense(a) => (0..n).all(|i| {
                (0..n).all(|j| {
                    let dot: f64 = (0..n)
                        .map(|k| f64::from(a[i * n + k]) * f64::from(a[j * n + k]))
                        .sum();
                    (dot - if i == j { 1.0 } else { 0.0 }).abs() <= ORTHOGONAL_TOLERANCE
                })
            }),
        }
    }

    /// The Frobenius norm of `A`, an upper bound of its spectral norm.
    fn frobenius(&self) -> f64 {
        let n = self.lines.len() as f64;
        match &self.matrix {
            Matrix::Diagonal | Matrix::Householder | Matrix::Hadamard => libm::sqrt(n),
            Matrix::Circulant(c) => {
                libm::sqrt(n * c.iter().map(|&x| f64::from(x) * f64::from(x)).sum::<f64>())
            }
            Matrix::Dense(a) => libm::sqrt(a.iter().map(|&x| f64::from(x) * f64::from(x)).sum()),
        }
    }

    /// The largest gain of any line in any band (NaN or negative: infinity).
    fn max_gain(&self) -> f64 {
        let mut max = 0.0f64;
        for line in self.lines.iter() {
            for band in Band::ALL {
                let g = f64::from(line.absorption.gain(band));
                if g.is_nan() || g < 0.0 {
                    return f64::INFINITY;
                }
                max = max.max(g);
            }
        }
        max
    }

    /// The RT60 of the network's loop in `band`, in seconds.
    ///
    /// For a diagonal `A` each line is its own loop and the slowest governs:
    /// the largest `-3 m / (fs log10 g)`. For a mixing `A` the loop is all the
    /// lines at once, the length-weighted `-3 Σm / (fs Σ log10 g)`, exact when
    /// every line is set from one RT60. `m` is the effective length.
    #[must_use]
    pub fn rt60(&self, band: Band, rate: f64) -> Seconds {
        let lines = self.lines.iter().map(|l| {
            (
                f64::from(l.effective_length()),
                libm::log10(f64::from(l.absorption.gain(band))),
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
        Seconds(seconds as f32)
    }

    /// Whether every gain is 1 (within a tolerance) and `A` is orthogonal: a
    /// loop that loses nothing.
    #[must_use]
    pub fn is_lossless(&self) -> bool {
        !self.lines.is_empty()
            && self.orthogonal()
            && self.lines.iter().all(|l| {
                Band::ALL
                    .iter()
                    .all(|&b| (f64::from(l.absorption.gain(b)) - 1.0).abs() <= LOSSLESS_TOLERANCE)
            })
    }

    /// Whether the loop cannot grow: every gain below 1 and `A` either
    /// orthogonal or small enough by the Frobenius bound (sufficient, not
    /// necessary), or the lossless case, marginally stable.
    #[must_use]
    pub fn is_stable(&self) -> bool {
        let n = self.lines.len();
        if !self.matrix_fits(n) {
            return false;
        }
        let diffusers_hold = self.lines.iter().all(|l| {
            l.diffuser
                .is_none_or(|d| d.gain.abs() < 1.0 && d.delay >= 0.0)
        });
        if !diffusers_hold {
            return false;
        }
        let g = self.max_gain();
        if self.is_lossless() {
            return g <= 1.0 + LOSSLESS_TOLERANCE;
        }
        g < 1.0 && (self.orthogonal() || g * self.frobenius() < 1.0)
    }

    /// The longest effective line, in frames.
    fn transit(&self) -> f64 {
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
    #[must_use]
    pub fn rt60(&self, band: Band) -> Seconds {
        let rate = self.rate.get();
        let mut slowest = 0.0f32;
        for stage in self.stages.iter() {
            let r = match stage {
                Stage::Fdn(f) => f.rt60(band, rate).get(),
                Stage::Fir(s) => s.rt60[band.index()].get(),
                _ => 0.0,
            };
            slowest = slowest.max(r);
        }
        Seconds(slowest)
    }

    /// How long the wet signal rings after the input stops, until it is `floor`
    /// below what it carried: the pre-delay, plus each stage's transit (the
    /// longest line, the longest tap, an all-pass's delay times the trips of its
    /// train, a FIR's length), plus the slowest band's `RT60 |floor| / 60`.
    ///
    /// [`Ring::Unbounded`] when a loop never settles (lossless, or a gain at 1
    /// or above), or when `floor` is NaN. A positive `floor` is read as 0 dB.
    #[must_use]
    pub fn tail(&self, floor: Db) -> Ring {
        if floor.get().is_nan() {
            return Ring::Unbounded;
        }
        let depth = f64::from((-floor.get()).max(0.0));
        let amplitude = libm::exp10(-depth / 20.0);
        let mut decay = 0.0f64;
        let mut frames = f64::from(self.pre_delay);
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
                        decay = decay.max(f64::from(f.rt60(band, self.rate.get()).get()));
                    }
                }
                Stage::Fir(s) => frames += s.length.get() as f64,
            }
        }
        if !frames.is_finite() || !decay.is_finite() {
            return Ring::Unbounded;
        }
        Ring::Finite(Seconds(
            (frames / self.rate.get() + decay * depth / 60.0) as f32,
        ))
    }

    /// Whether no loop can grow. Every recirculating stage must be stable
    /// ([`Fdn::is_stable`]), every series all-pass must have `|g| < 1`, and the
    /// figures must be finite.
    #[must_use]
    pub fn is_stable(&self) -> bool {
        self.pre_delay.is_finite()
            && self.pre_delay >= 0.0
            && self.stages.iter().all(|s| match s {
                Stage::Fdn(f) => f.is_stable(),
                Stage::AllPass(a) => a.iter().all(|a| a.gain.abs() < 1.0 && a.delay >= 0.0),
                Stage::Taps(t) => t.taps.iter().all(|t| t.gain.is_finite() && t.delay >= 0.0),
                Stage::Fir(s) => s.rt60.iter().all(|r| !r.get().is_nan()),
            })
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
        Seconds((frames / self.rate.get()) as f32)
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
        let t = f64::from(at.get().max(0.0));
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

    /// The processing latency: the largest of the FIR stages' (zero for the
    /// time-domain stages).
    #[must_use]
    pub fn latency(&self) -> Samples {
        self.stages
            .iter()
            .filter_map(|s| match s {
                Stage::Fir(s) => Some(s.latency),
                _ => None,
            })
            .max()
            .unwrap_or(Samples::ZERO)
    }

    /// The shortest path from the input to the output, in frames: the
    /// pre-delay plus each stage's shortest path (a series all-pass has a
    /// direct term, zero; a line, its length; a tap, its delay; a FIR, its
    /// onset). Infinity when no path exists.
    #[must_use]
    pub fn min_delay(&self) -> f32 {
        let mut frames = self.pre_delay;
        for stage in self.stages.iter() {
            frames += match stage {
                Stage::Taps(t) => t.taps.iter().map(|t| t.delay).fold(f32::INFINITY, f32::min),
                Stage::AllPass(_) => 0.0,
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
mod tests {
    use tutti_types::SampleRate;

    use super::super::{
        Absorption, AllPass, Band, Bounds, Fdn, FirSummary, Gains, Inline, Line, Matrix,
        Modulation, Network, OutputMap, Stage, Tap, Taps, loop_gain,
    };
    use super::*;

    const RATE: SampleRate = SampleRate(48_000.0);

    fn line(length: f32, absorption: Absorption) -> Line {
        Line {
            length,
            modulation: Modulation::NONE,
            absorption,
            diffuser: None,
        }
    }

    fn fdn(lines: &[Line], matrix: Matrix) -> Fdn {
        Fdn {
            lines: Inline::from_slice(lines).unwrap(),
            matrix,
            input: Gains::filled(1.0, lines.len()).unwrap(),
            output: OutputMap::default(),
        }
    }

    fn network(stages: &[Stage]) -> Network {
        let mut n = Network::new(RATE);
        for s in stages {
            n.stages.push(s.clone()).unwrap();
        }
        n
    }

    /// One 1 s line (48 000 frames) set for an RT60 of 2 s, after a 0.1 s
    /// pre-delay: the tail to -60 dB is `0.1 + 1 + 2` = 3.1 s, to -30 dB
    /// `0.1 + 1 + 1` = 2.1 s, and to -90 dB 4.1 s.
    ///
    /// Mutations (run): the pre-delay initial `f64::from(self.pre_delay)`
    /// replaced by 0 fails the first (3.0); `/ 60.0` -> `/ 30.0` fails the
    /// second; `transit` replaced by 0 fails all.
    #[test]
    fn the_tail_is_pre_delay_transit_and_the_decay_to_the_floor() {
        let g = loop_gain(48_000.0, Seconds(2.0), RATE);
        let mut n = network(&[Stage::Fdn(fdn(
            &[line(48_000.0, Absorption::Gain(g))],
            Matrix::Diagonal,
        ))]);
        n.pre_delay = 4_800.0;
        let seconds = |floor| n.tail(Db(floor)).seconds().unwrap().get();
        assert!((seconds(-60.0) - 3.1).abs() < 1e-5, "{}", seconds(-60.0));
        assert!((seconds(-30.0) - 2.1).abs() < 1e-5);
        assert!((seconds(-90.0) - 4.1).abs() < 1e-5);
        assert!((n.rt60(Band::Mid).get() - 2.0).abs() < 1e-5);
    }

    /// A series all-pass rings for its delay times the trips its train needs to
    /// fall to the floor: at `g = 0.5` to -60 dB that is `ceil(ln 1e-3 /
    /// ln 0.5)` = 10 trips after the first, so 11 delays. It adds no RT60.
    ///
    /// Mutation (run): dropping the `1.0 +` in `all_pass_ring` gives 1000
    /// frames and fails.
    #[test]
    fn a_series_all_pass_rings_for_its_geometric_train() {
        let ap = AllPass {
            delay: 100.0,
            gain: 0.5,
            modulation: Modulation::NONE,
        };
        let n = network(&[Stage::AllPass(Inline::from_slice(&[ap, ap]).unwrap())]);
        let t = n.tail(Db(-60.0)).seconds().unwrap().get();
        assert!((t - 1100.0 / 48_000.0).abs() < 1e-7, "{t}");
        assert_eq!(n.rt60(Band::Low).get(), 0.0);
        assert!(n.is_stable() && !n.is_lossless());
    }

    /// For a diagonal `A` the slowest line governs; for a mixing `A` the
    /// loop is all the lines at once, `-3 Σm / (fs Σ log10 g)`, which for lines
    /// each set from RT60 `T` is `T`, and for lines set for different RT60s is
    /// between them.
    ///
    /// Mutations (run): `f64::max` -> `f64::min` in the diagonal fold gives
    /// 0 and fails; averaging the logs in the mixing arm fails the second pair.
    #[test]
    fn rt60_is_the_slowest_line_for_a_diagonal_and_the_length_weighted_loop_for_a_mixing() {
        let slow = line(
            40_000.0,
            Absorption::Gain(loop_gain(40_000.0, Seconds(3.0), RATE)),
        );
        let fast = line(
            20_000.0,
            Absorption::Gain(loop_gain(20_000.0, Seconds(1.0), RATE)),
        );
        let diagonal = network(&[Stage::Fdn(fdn(&[slow, fast], Matrix::Diagonal))]);
        assert!((diagonal.rt60(Band::High).get() - 3.0).abs() < 1e-4);
        // Mixed: -3 (40000 + 20000) / (48000 (log10 g1 + log10 g2)) with
        // log10 g = -3 m / (T fs): = (m1 + m2) / (m1/T1 + m2/T2) = 60000 / (40000/3 + 20000).
        let mixed = network(&[Stage::Fdn(fdn(&[slow, fast], Matrix::Hadamard))]);
        let want = 60_000.0 / (40_000.0 / 3.0 + 20_000.0);
        assert!(
            (f64::from(mixed.rt60(Band::High).get()) - want).abs() < 1e-4,
            "{:?}",
            mixed.rt60(Band::High)
        );
        let equal = [20_000.0, 30_000.0]
            .map(|m| line(m, Absorption::Gain(loop_gain(m, Seconds(2.0), RATE))));
        let same = network(&[Stage::Fdn(fdn(&equal, Matrix::Householder))]);
        assert!((same.rt60(Band::Low).get() - 2.0).abs() < 1e-4);
    }

    /// The bands of a one-pole at pole 0.5 and gain 0.8: DC `0.8`, a quarter of
    /// the rate `0.8 * 0.5 / sqrt(1.25)`, Nyquist `0.8 * 0.5 / 1.5`; explicit
    /// bands read straight.
    ///
    /// Mutation (run): `1.0 + d * d` -> `1.0 - d * d` in the mid response fails.
    #[test]
    fn a_one_pole_has_its_gain_at_dc_a_quarter_rate_and_nyquist() {
        let a = Absorption::OnePole {
            gain: 0.8,
            damp: 0.5,
        };
        assert!((a.gain(Band::Low) - 0.8).abs() < 1e-7);
        assert!((a.gain(Band::Mid) - 0.8 * 0.5 / 1.25f32.sqrt()).abs() < 1e-6);
        assert!((a.gain(Band::High) - 0.8 * 0.5 / 1.5).abs() < 1e-6);
        let b = Absorption::Bands {
            low: 0.9,
            mid: 0.8,
            high: 0.7,
            crossover: [tutti_types::Hz(200.0), tutti_types::Hz(2000.0)],
        };
        assert_eq!(Band::ALL.map(|x| b.gain(x)), [0.9, 0.8, 0.7]);
        let from = Absorption::from_rt60(
            [Seconds(2.0), Seconds(1.0), Seconds(0.5)],
            [tutti_types::Hz(200.0), tutti_types::Hz(2000.0)],
            4800.0,
            RATE,
        );
        let want = [2.0, 1.0, 0.5].map(|t| loop_gain(4800.0, Seconds(t), RATE));
        assert_eq!(Band::ALL.map(|x| from.gain(x)), want);
    }

    /// A nested all-pass adds its group delay at DC, `D (1 + g) / (1 - g)`, to
    /// the line's length: 100 frames at 0.5 add 300, and the RT60 follows the
    /// longer loop.
    ///
    /// Mutation (run): `1.0 + self.gain` -> `1.0 - self.gain` gives 100/3 and
    /// fails.
    #[test]
    fn a_nested_all_pass_lengthens_the_loop_by_its_low_frequency_delay() {
        let ap = AllPass {
            delay: 100.0,
            gain: 0.5,
            modulation: Modulation::NONE,
        };
        assert_eq!(ap.low_frequency_delay(), 300.0);
        assert_eq!(
            AllPass { gain: 1.0, ..ap }.low_frequency_delay(),
            f32::INFINITY
        );
        let g = 0.9;
        let plain = line(1000.0, Absorption::Gain(g));
        let nested = Line {
            diffuser: Some(ap),
            ..plain
        };
        assert_eq!(nested.effective_length(), 1300.0);
        let r = |l| {
            network(&[Stage::Fdn(fdn(&[l], Matrix::Diagonal))])
                .rt60(Band::Mid)
                .get()
        };
        assert!((r(nested) / r(plain) - 1.3).abs() < 1e-5);
    }

    /// Stability: a gain below 1 under an orthogonal matrix is stable; a gain
    /// of 1 in one band only is not (it never settles) and its tail is
    /// unbounded; a Hadamard of 3 lines, a dense matrix of the wrong size, a
    /// non-orthogonal circulant with `g ||A||_F >= 1`, and a negative or NaN
    /// gain are not; all-pass gains of 1 or more are not.
    ///
    /// Mutations (run): `g < 1.0` -> `g <= 1.0` in `is_stable` makes the
    /// one-band-at-1 network stable and fails; dropping `matrix_fits` fails
    /// the Hadamard of 3.
    #[test]
    fn stability_follows_the_gains_and_the_matrix() {
        let stable = |lines: &[Line], m| network(&[Stage::Fdn(fdn(lines, m))]).is_stable();
        let two = |g| {
            [
                line(100.0, Absorption::Gain(g)),
                line(130.0, Absorption::Gain(g)),
            ]
        };
        for m in [Matrix::Diagonal, Matrix::Householder, Matrix::Hadamard] {
            assert!(stable(&two(0.99), m.clone()));
            assert!(!stable(&two(1.01), m));
        }
        let low_only = Absorption::Bands {
            low: 1.0,
            mid: 0.9,
            high: 0.9,
            crossover: [tutti_types::Hz(200.0), tutti_types::Hz(2000.0)],
        };
        let n = network(&[Stage::Fdn(fdn(
            &[line(100.0, low_only), line(130.0, low_only)],
            Matrix::Hadamard,
        ))]);
        assert!(!n.is_stable() && !n.is_lossless());
        assert_eq!(n.tail(Db(-60.0)), Ring::Unbounded);
        let three = [100.0, 130.0, 170.0].map(|m| line(m, Absorption::Gain(0.5)));
        assert!(!stable(&three, Matrix::Hadamard));
        assert!(stable(&three, Matrix::Householder));
        assert!(!stable(
            &two(0.5),
            Matrix::Dense(Inline::from_slice(&[1.0, 0.0, 0.0]).unwrap())
        ));
        // A shift is an orthogonal circulant; [1, 1] is not, and is stable only
        // when g times its Frobenius norm 2 is below 1.
        let shift = Matrix::Circulant(Inline::from_slice(&[0.0, 1.0]).unwrap());
        assert!(stable(&two(0.99), shift));
        let ones = Matrix::Circulant(Inline::from_slice(&[1.0, 1.0]).unwrap());
        assert!(!stable(&two(0.6), ones.clone()));
        assert!(stable(&two(0.4), ones));
        let dense_orthogonal = Matrix::Dense(Inline::from_slice(&[0.0, 1.0, -1.0, 0.0]).unwrap());
        assert!(stable(&two(0.99), dense_orthogonal));
        let dense_big = Matrix::Dense(Inline::from_slice(&[0.0, 2.0, 2.0, 0.0]).unwrap());
        assert!(!stable(&two(0.6), dense_big));
        assert!(!stable(
            &[line(100.0, Absorption::Gain(-0.5))],
            Matrix::Diagonal
        ));
        assert!(!stable(
            &[line(100.0, Absorption::Gain(f32::NAN))],
            Matrix::Diagonal
        ));
        let ap = |gain| {
            network(&[Stage::AllPass(
                Inline::from_slice(&[AllPass {
                    delay: 10.0,
                    gain,
                    modulation: Modulation::NONE,
                }])
                .unwrap(),
            )])
        };
        assert!(ap(0.9).is_stable() && !ap(1.0).is_stable() && !ap(-1.5).is_stable());
    }

    /// Every gain 1 under an orthogonal matrix is lossless and marginally
    /// stable, with an unbounded tail; one band at 0.999 is not.
    ///
    /// Mutation (run): the tolerance `1e-6` -> `1e-2` calls 0.999 lossless and
    /// fails the last assertion.
    #[test]
    fn a_lossless_loop_is_marginally_stable_with_an_unbounded_tail() {
        let one = Absorption::Gain(1.0);
        let frozen = network(&[Stage::Fdn(fdn(
            &[line(100.0, one), line(130.0, one)],
            Matrix::Hadamard,
        ))]);
        assert!(frozen.is_lossless() && frozen.is_stable());
        assert_eq!(frozen.tail(Db(-60.0)), Ring::Unbounded);
        assert_eq!(frozen.rt60(Band::Mid).get(), f32::INFINITY);
        let almost = Absorption::Bands {
            low: 1.0,
            mid: 1.0,
            high: 0.999,
            crossover: [tutti_types::Hz(200.0), tutti_types::Hz(2000.0)],
        };
        let n = network(&[Stage::Fdn(fdn(
            &[line(100.0, almost), line(130.0, almost)],
            Matrix::Hadamard,
        ))]);
        assert!(!n.is_lossless());
        assert_eq!(n.tail(Db(f32::NAN)), Ring::Unbounded);
        assert!(!network(&[]).is_lossless());
    }

    /// Modal density is the total delay in seconds. Echo density at 1 kHz: a
    /// diagonal of lines 10 and 20 frames long has nothing before 10 ms, 100 per
    /// second from then, 150 from 20 ms (the times are a little past the line
    /// lengths, as `Seconds` is `f32`); a Hadamard of the same lines multiplies
    /// each trip (mean 15 ms): `2^1 / 0.015` at 10 ms, `2^3 / 0.015` at 45 ms,
    /// capped at the rate.
    ///
    /// Mutations (run): dropping the `tau <= t` filter fails the first assert;
    /// `libm::pow(n, trips)` -> `n * trips` fails the 45 ms figure.
    #[test]
    fn the_densities_are_the_modes_per_hertz_and_the_echoes_per_second() {
        let slow = SampleRate(1_000.0);
        let ls = [
            line(10.0, Absorption::Gain(0.5)),
            line(20.0, Absorption::Gain(0.5)),
        ];
        let mut diag = Network::new(slow);
        diag.stages
            .push(Stage::Fdn(fdn(&ls, Matrix::Diagonal)))
            .unwrap();
        assert!((diag.modal_density().get() - 0.03).abs() < 1e-7);
        let e = |n: &Network, t: f32| n.echo_density(Seconds(t)).get();
        assert_eq!(e(&diag, 0.005), 0.0);
        assert!((e(&diag, 0.0105) - 100.0).abs() < 1e-3);
        assert!((e(&diag, 0.0205) - 150.0).abs() < 1e-3);
        assert!(
            (e(&diag, 5.0) - 150.0).abs() < 1e-3,
            "a diagonal never densifies"
        );
        let mut mixed = Network::new(slow);
        mixed
            .stages
            .push(Stage::Fdn(fdn(&ls, Matrix::Hadamard)))
            .unwrap();
        assert_eq!(e(&mixed, 0.005), 0.0);
        assert!(
            (e(&mixed, 0.0105) - 2.0 / 0.015).abs() < 0.01,
            "{}",
            e(&mixed, 0.0105)
        );
        assert!(
            (e(&mixed, 0.0455) - 8.0 / 0.015).abs() < 0.1,
            "{}",
            e(&mixed, 0.0455)
        );
        assert_eq!(e(&mixed, 5.0), 1_000.0, "capped at one echo per frame");
        let fir = FirSummary {
            length: Samples(500),
            onset: Samples(0),
            latency: Samples(64),
            rt60: [Seconds(1.0); 3],
            energy: 1.0,
        };
        let mut f = Network::new(slow);
        f.stages.push(Stage::Fir(fir)).unwrap();
        assert_eq!(e(&f, 0.1), 1_000.0);
        assert_eq!(e(&f, 0.6), 0.0);
        assert_eq!(f.modal_density().get(), 0.5);
    }

    /// A FIR reports its summary for `rt60` and its latency; the time-domain
    /// stages have none; its length is its tail.
    ///
    /// Mutation (run): `Stage::Fir(s) => frames += length` replaced by `0.0`
    /// fails the tail.
    #[test]
    fn a_fir_reports_its_summary() {
        let fir = FirSummary {
            length: Samples(24_000),
            onset: Samples(100),
            latency: Samples(256),
            rt60: [Seconds(2.0), Seconds(1.5), Seconds(1.0)],
            energy: 1.0,
        };
        let n = network(&[Stage::Fir(fir)]);
        assert_eq!(Band::ALL.map(|b| n.rt60(b).get()), [2.0, 1.5, 1.0]);
        assert_eq!(n.latency(), Samples(256));
        assert_eq!(n.tail(Db(-60.0)), Ring::Finite(Seconds(0.5)));
        assert_eq!(n.min_delay(), 100.0);
        assert_eq!(network(&[]).latency(), Samples::ZERO);
    }

    /// The shortest path: a series all-pass has a direct term, a tap or a line
    /// its delay, the pre-delay adds; a network with no path has none to find.
    ///
    /// Mutation (run): `Stage::AllPass(_) => 0.0` -> `a.delay` fails the
    /// all-pass cases.
    #[test]
    fn the_shortest_path_decides_the_direct_path() {
        let ap = Stage::AllPass(
            Inline::from_slice(&[AllPass {
                delay: 100.0,
                gain: 0.5,
                modulation: Modulation::NONE,
            }])
            .unwrap(),
        );
        assert!(network(std::slice::from_ref(&ap)).has_direct_path());
        let comb = Stage::Fdn(fdn(&[line(100.0, Absorption::Gain(0.5))], Matrix::Diagonal));
        let mut n = network(&[comb.clone(), ap]);
        assert_eq!(n.min_delay(), 100.0);
        assert!(!n.has_direct_path());
        n.pre_delay = 20.0;
        assert_eq!(n.min_delay(), 120.0);
        let tap = |d| {
            Stage::Taps(Taps {
                taps: Inline::from_slice(&[Tap {
                    delay: d,
                    gain: 1.0,
                    channel: 0,
                }])
                .unwrap(),
            })
        };
        assert!(network(&[tap(0.5)]).has_direct_path());
        assert!(!network(&[tap(1.0)]).has_direct_path());
        assert_eq!(network(&[tap(7.0), comb]).min_delay(), 107.0);
        assert_eq!(
            network(&[]).min_delay(),
            0.0,
            "no stages: nothing is added to the pre-delay"
        );
    }

    /// `Bounds::dominates` needs every figure, and `of` measures them.
    ///
    /// Mutation (run): dropping any one `>=` from `dominates` fails the
    /// matching row of the loop.
    #[test]
    fn a_bound_dominates_only_when_every_figure_does() {
        let g = Absorption::Gain(0.5);
        let mut f = fdn(
            &[
                line(100.0, g),
                Line {
                    modulation: Modulation {
                        depth: 2.0,
                        rate: tutti_types::Hz(1.0),
                    },
                    ..line(200.0, g)
                },
            ],
            Matrix::Hadamard,
        );
        f.output.rows.push(Gains::filled(1.0, 2).unwrap()).unwrap();
        let mut n = network(&[Stage::Fdn(f)]);
        n.pre_delay = 10.0;
        let b = Bounds::of(&n, Db(-60.0));
        assert_eq!((b.lines, b.stages, b.taps, b.outputs), (2, 1, 0, 1));
        assert_eq!((b.line, b.delay, b.pre_delay), (202.0, 302.0, 10.0));
        assert!(b.dominates(&b));
        let smaller: [fn(&mut Bounds); 8] = [
            |b| b.lines -= 1,
            |b| b.stages -= 1,
            |b| b.outputs -= 1,
            |b| b.line -= 1.0,
            |b| b.delay -= 1.0,
            |b| b.pre_delay -= 1.0,
            |b| b.tail = Ring::Finite(Seconds(0.0)),
            |b| b.ir = Samples::ZERO,
        ];
        let mut with_taps_and_ir = b;
        with_taps_and_ir.taps = 1;
        with_taps_and_ir.ir = Samples(5);
        for (i, shrink) in smaller.iter().enumerate() {
            let mut s = if i == 7 { with_taps_and_ir } else { b };
            shrink(&mut s);
            let (big, small) = if i == 7 {
                (&with_taps_and_ir, &s)
            } else {
                (&b, &s)
            };
            assert!(big.dominates(small), "row {i} the other way round");
            assert!(!small.dominates(big), "row {i}");
        }
        assert!(!b.dominates(&with_taps_and_ir), "taps");
    }
}
