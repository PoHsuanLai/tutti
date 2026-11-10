//! The Linkwitz-Riley crossover as a model: [`CrossoverModel`], the design
//! (the splits' coefficients at a rate), separated from the state that runs it
//! ([`Crossover`](crate::Crossover)).
//!
//! A band of an `n`-split tree is `HP^2` of every earlier split, `LP^2` of its
//! own and the all-pass of every later one (see `crossover.rs`), so each band's
//! response is the product of those, in closed form.

use core::f64::consts::{FRAC_1_SQRT_2, PI};

use tutti_types::{Db, Hz, SampleRate, Samples};

use super::biquad::{Biquad, decay_ring, section_l1};
use super::crossover::{CrossoverError, MAX_SPLITS, SOLVE_CEILING, validate};
use crate::{Complex64, Lti, Ring, System};

/// The three biquads one split needs, from one warped frequency.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Split {
    pub(super) low: Biquad,
    pub(super) high: Biquad,
    pub(super) all_pass: Biquad,
}

impl Split {
    /// The Butterworth (Q = 1/sqrt(2)) low-pass, high-pass and all-pass at
    /// `hz` against `rate` (bilinear transform).
    pub(super) fn solve(hz: f64, rate: f64) -> Self {
        let hz = hz.min(SOLVE_CEILING * rate);
        let k = libm::tan(PI * hz / rate);
        let k2 = k * k;
        let norm = 1.0 / (1.0 + k / FRAC_1_SQRT_2 + k2);
        let a1 = 2.0 * (k2 - 1.0) * norm;
        let a2 = (1.0 - k / FRAC_1_SQRT_2 + k2) * norm;
        let low = k2 * norm;
        Self {
            low: Biquad {
                b0: low,
                b1: 2.0 * low,
                b2: low,
                a1,
                a2,
            },
            high: Biquad {
                b0: norm,
                b1: -2.0 * norm,
                b2: norm,
                a1,
                a2,
            },
            all_pass: Biquad {
                b0: a2,
                b1: a1,
                b2: 1.0,
                a1,
                a2,
            },
        }
    }
}

/// The design of a Linkwitz-Riley crossover: its sample rate, the frequencies its
/// coefficients are solved at and those coefficients. A value without state; the
/// crossover that runs it keeps the integrators and the ramp between two of these.
///
/// It is an [`Lti`] with one input and one output a band, lowest first: the bands
/// are at -6.02 dB at their crossover and sum to unit magnitude.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CrossoverModel {
    pub(super) rate: f64,
    pub(super) bands: usize,
    pub(super) freqs: [f64; MAX_SPLITS],
    pub(super) coeffs: [Split; MAX_SPLITS],
}

impl CrossoverModel {
    /// A model with a split at each of `frequencies` at `rate`, so
    /// `frequencies.len() + 1` bands.
    ///
    /// # Errors
    ///
    /// [`CrossoverError`] as [`Crossover::new`](crate::Crossover::new): a band count
    /// outside 2 to 6, a rate that is not finite and positive, or frequencies that
    /// are not finite, above 0, strictly increasing and below Nyquist.
    pub fn new(frequencies: &[Hz], rate: impl Into<SampleRate>) -> Result<Self, CrossoverError> {
        let rate = rate.into();
        validate(frequencies, rate)?;
        let mut model = Self::unsolved(frequencies.len() + 1, rate.get());
        for (f, hz) in model.freqs.iter_mut().zip(frequencies) {
            *f = f64::from(hz.get());
        }
        model.solve();
        Ok(model)
    }

    /// `bands` bands at `rate`, the coefficients not yet solved for any frequency.
    pub(super) fn unsolved(bands: usize, rate: f64) -> Self {
        Self {
            rate,
            bands,
            freqs: [0.0; MAX_SPLITS],
            coeffs: [Split::solve(1_000.0, 48_000.0); MAX_SPLITS],
        }
    }

    /// Solves every split's coefficients at `self.freqs`.
    pub(super) fn solve(&mut self) {
        for j in 0..self.splits() {
            self.coeffs[j] = Split::solve(self.freqs[j], self.rate);
        }
    }

    /// How many bands it makes.
    #[must_use]
    pub fn bands(&self) -> usize {
        self.bands
    }

    /// Crossover `i`'s frequency as solved, or `None` past the last.
    #[must_use]
    pub fn frequency(&self, i: usize) -> Option<Hz> {
        (i < self.splits()).then(|| Hz(self.freqs[i] as f32))
    }

    /// The sample rate the coefficients are solved at.
    #[must_use]
    pub fn sample_rate(&self) -> SampleRate {
        SampleRate(self.rate)
    }

    fn splits(&self) -> usize {
        self.bands - 1
    }
}

impl System for CrossoverModel {
    /// Zero: the bands are not delayed.
    fn latency(&self) -> Samples {
        Samples::ZERO
    }

    /// The slowest split's pole ring, with the double pole of its squared
    /// sections (`n r^n`).
    fn tail(&self, floor: Db) -> Ring {
        decay_ring(self.spectral_radius(), 2, floor, self.sample_rate())
    }
}

impl Lti for CrossoverModel {
    fn outputs(&self) -> usize {
        self.bands
    }

    /// Band `output`: `HP^2` of every split before it, `LP^2` of its own and the
    /// all-pass of every split after. NaN past the last band.
    fn frequency_response(&self, output: usize, freqs: &[Hz], gain: &mut [Complex64]) {
        for (g, f) in gain.iter_mut().zip(freqs) {
            *g = if output < self.bands {
                let w = 2.0 * PI * f64::from(f.get()) / self.rate;
                let mut h = Complex64::new(1.0, 0.0);
                for (j, c) in self.coeffs[..self.splits()].iter().enumerate() {
                    if j < output {
                        let hp = c.high.response(w);
                        h *= hp * hp;
                    } else if j == output {
                        let lp = c.low.response(w);
                        h *= lp * lp;
                    } else {
                        h *= c.all_pass.response(w);
                    }
                }
                h
            } else {
                Complex64::new(f64::NAN, f64::NAN)
            };
        }
    }

    /// The slowest section's pole radius (every section of one split shares its
    /// poles).
    fn spectral_radius(&self) -> f64 {
        self.coeffs[..self.splits()]
            .iter()
            .map(|c| c.low.spectral_radius())
            .fold(0.0, f64::max)
    }

    /// The largest band's product of its sections' l1 bounds.
    fn l1_norm(&self) -> f64 {
        let n = self.splits();
        (0..self.bands)
            .map(|band| {
                (0..n)
                    .map(|j| {
                        let c = &self.coeffs[j];
                        match j.cmp(&band) {
                            std::cmp::Ordering::Less => section_l1(&c.high).powi(2),
                            std::cmp::Ordering::Equal => section_l1(&c.low).powi(2),
                            std::cmp::Ordering::Greater => section_l1(&c.all_pass),
                        }
                    })
                    .product::<f64>()
            })
            .fold(0.0, f64::max)
    }

    /// 1: every band is the product of Butterworth low-passes and high-passes
    /// (each under 1 everywhere, so each squared too) and all-passes (exactly 1),
    /// and the band that holds DC (or Nyquist) reaches it.
    fn hinf_norm(&self) -> f64 {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Complex32, Crossover, RealFft};

    const RATE: SampleRate = SampleRate(48_000.0);

    fn model(freqs: &[f32]) -> CrossoverModel {
        let f: Vec<Hz> = freqs.iter().map(|&f| Hz(f)).collect();
        CrossoverModel::new(&f, RATE).unwrap()
    }

    fn at(m: &CrossoverModel, band: usize, f: f32) -> Complex64 {
        let mut g = [Complex64::new(0.0, 0.0)];
        m.frequency_response(band, &[Hz(f)], &mut g);
        g[0]
    }

    /// The closed forms of Linkwitz and Riley: each adjacent pair of bands is
    /// -6.02 dB (one half) at its crossover, and the bands sum to a unit-magnitude
    /// all-pass at every frequency (the sum is the product of the splits'
    /// all-passes), for two to six bands.
    ///
    /// Mutations (run): a band missing one all-pass of a later split fails the
    /// sum; `lp * lp` taken as `lp` fails the half at the crossover.
    #[test]
    fn the_bands_are_6_db_down_at_the_crossover_and_sum_to_an_all_pass() {
        for freqs in [
            vec![1_000.0f32],
            vec![200.0, 2_000.0],
            vec![100.0, 500.0, 2_500.0],
            vec![80.0, 300.0, 1_200.0, 4_000.0, 12_000.0],
        ] {
            let m = model(&freqs);
            for (j, &fc) in freqs.iter().enumerate() {
                for band in [j, j + 1] {
                    let g = at(&m, band, fc);
                    // Only the neighbouring splits' influence away from fc is a
                    // product of all-passes, so the magnitude at fc is exactly
                    // that of the Butterworth squared.
                    assert!(
                        (g.norm() - 0.5).abs() < 1e-3 || freqs.len() > 1,
                        "{freqs:?} band {band}: {}",
                        g.norm()
                    );
                }
                let low = at(&m, j, fc).norm();
                let high = at(&m, j + 1, fc).norm();
                // The two bands that meet at `fc` add in phase to the all-pass'
                // part, each a half, up to the other splits' all-passes.
                assert!((low - high).abs() < 0.1 * low.max(high) + 1e-3, "{freqs:?}");
            }
            for f in [10.0f32, 90.0, 400.0, 1_500.0, 6_000.0, 20_000.0, 24_000.0] {
                let sum: Complex64 = (0..m.bands()).map(|b| at(&m, b, f)).sum();
                assert!(
                    (sum.norm() - 1.0).abs() < 1e-9,
                    "{freqs:?} at {f}: {}",
                    sum.norm()
                );
            }
        }
    }

    /// Two bands, exactly: both bands are 1/2 at the crossover, the low band is 1 at
    /// DC and the high band 1 at Nyquist (the closed form of `LP^2` and `HP^2`).
    ///
    /// Mutation (run): `Split::solve`'s Q of 0.5 instead of `FRAC_1_SQRT_2` changes
    /// the half at the crossover and fails.
    #[test]
    fn two_bands_have_the_closed_form_anchors() {
        let m = model(&[1_000.0]);
        assert!((at(&m, 0, 1_000.0).norm() - 0.5).abs() < 1e-12);
        assert!((at(&m, 1, 1_000.0).norm() - 0.5).abs() < 1e-12);
        assert!((at(&m, 0, 0.0).norm() - 1.0).abs() < 1e-12);
        assert!((at(&m, 1, 24_000.0).norm() - 1.0).abs() < 1e-9);
    }

    /// The model's response is the running crossover's: the `RealFft` of each
    /// band's impulse response through `Crossover::split` against
    /// `frequency_response`, to 1e-5 of its norm, for three bands.
    ///
    /// Mutation (run): a band's all-pass applied twice in the model fails it.
    #[test]
    fn the_response_is_what_the_crossover_runs() {
        const N: usize = 1 << 15;
        let freqs = [Hz(300.0), Hz(3_000.0)];
        let m = CrossoverModel::new(&freqs, RATE).unwrap();
        let mut xo = Crossover::new(&freqs, RATE).unwrap();
        let mut bands = vec![vec![0.0f32; N]; 3];
        for n in 0..N {
            let out = xo.split(if n == 0 { 1.0 } else { 0.0 });
            for (b, v) in bands.iter_mut().zip(out) {
                b[n] = v;
            }
        }
        let fft = RealFft::new(N);
        let mut scratch = fft.new_scratch();
        for (band, ir) in bands.iter().enumerate() {
            let mut bins = vec![Complex32::new(0.0, 0.0); fft.bins()];
            fft.forward(ir, &mut bins, &mut scratch);
            let (mut num, mut den) = (0.0f64, 0.0f64);
            for (i, b) in bins.iter().enumerate().step_by(5) {
                let want = at(&m, band, (i as f64 * RATE.get() / N as f64) as f32);
                let got = Complex64::new(f64::from(b.re), f64::from(b.im));
                num += (got - want).norm_sqr();
                den += want.norm_sqr();
            }
            assert!(
                (num / den).sqrt() < 1e-5,
                "band {band}: {}",
                (num / den).sqrt()
            );
        }
    }

    /// The radius is the Butterworth section's pole modulus, from its own closed
    /// form `|p|^2 = a2 = (1 - sqrt2 k + k^2) / (1 + sqrt2 k + k^2)` with
    /// `k = tan(pi fc / rate)`, of the lowest split (the slowest).
    ///
    /// Mutation (run): taking the highest split's fails.
    #[test]
    fn the_radius_is_the_lowest_splits_butterworth_pole() {
        let m = model(&[60.0, 800.0, 5_000.0]);
        let k = libm::tan(PI * 60.0 / 48_000.0);
        let a2 = (1.0 - 2f64.sqrt() * k + k * k) / (1.0 + 2f64.sqrt() * k + k * k);
        assert!((m.spectral_radius() - a2.sqrt()).abs() < 1e-12);
    }

    /// The ring: the lowest band's measured time to -60 dB of its peak is covered
    /// by the model's tail and within three times it, and 60 s of noise through
    /// every band stays under `l1_norm` and the response under `hinf_norm`.
    ///
    /// Mutations (run): the tail halved fails the cover, quadrupled the slack;
    /// `l1_norm` scaled by 0.1 fails the noise.
    #[test]
    fn the_tail_covers_the_measured_ring_and_the_norms_bound_the_output() {
        const N: usize = 1 << 17;
        let freqs = [Hz(80.0), Hz(2_000.0)];
        let m = CrossoverModel::new(&freqs, RATE).unwrap();
        let mut xo = Crossover::new(&freqs, RATE).unwrap();
        let ir: Vec<f32> = (0..N)
            .map(|n| xo.split(if n == 0 { 1.0 } else { 0.0 })[0])
            .collect();
        let win = 256;
        let env: Vec<f64> = ir
            .chunks(win)
            .map(|c| (c.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>() / win as f64).sqrt())
            .collect();
        let peak = env.iter().copied().fold(0.0, f64::max);
        let last = env.iter().rposition(|&v| v >= peak * 1e-3).unwrap();
        let took = ((last + 1) * win) as f64 / RATE.get();
        let Ring::Finite(tail) = m.tail(Db(-60.0)) else {
            panic!("finite")
        };
        assert!(
            tail.get() >= took,
            "tail {} against measured {took}",
            tail.get()
        );
        assert!(
            tail.get() <= 3.0 * took,
            "tail {} against measured {took}",
            tail.get()
        );

        let mut xo = Crossover::new(&freqs, RATE).unwrap();
        let mut state = 7u32;
        let mut worst = 0.0f64;
        for _ in 0..(60 * 48_000) {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let x = (state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
            let out = xo.split(x);
            worst = out[..3]
                .iter()
                .fold(worst, |m, &s| m.max(f64::from(s.abs())));
        }
        assert!(worst <= m.l1_norm(), "{worst} against {}", m.l1_norm());
        assert!(m.hinf_norm() <= m.l1_norm());
    }

    /// The design is separate from the state: the model of a crossover is the
    /// model built from its frequencies, before and after it has run.
    ///
    /// Mutation (run): `Crossover::model` returning a model solved at the wrong
    /// frequencies (the unsolved one) fails.
    #[test]
    fn a_crossover_holds_its_model_apart_from_its_state() {
        let freqs = [Hz(250.0), Hz(4_000.0)];
        let mut xo = Crossover::new(&freqs, RATE).unwrap();
        let want = CrossoverModel::new(&freqs, RATE).unwrap();
        assert_eq!(xo.model(), &want);
        for n in 0..1000 {
            xo.split(if n % 7 == 0 { 0.5 } else { -0.2 });
        }
        assert_eq!(xo.model(), &want);
        xo.set_frequencies(&[Hz(300.0), Hz(5_000.0)]).unwrap();
        xo.split(0.1);
        assert_eq!(
            xo.model(),
            &CrossoverModel::new(&[Hz(300.0), Hz(5_000.0)], RATE).unwrap()
        );
    }
}
