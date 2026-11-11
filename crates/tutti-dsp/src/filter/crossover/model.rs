//! The Linkwitz-Riley crossover as a model: [`CrossoverModel`], the design
//! (the splits' coefficients at a rate), separated from the state that runs it
//! ([`Crossover`](crate::Crossover)).
//!
//! A band of an `n`-split tree is `HP^2` of every earlier split, `LP^2` of its
//! own and the all-pass of every later one (see `legacy.rs`), so each band's
//! response is the product of those, in closed form.

use core::f64::consts::{FRAC_1_SQRT_2, PI};

use tutti_types::{Db, Hz, SampleRate, Samples};

use super::legacy::{CrossoverError, MAX_SPLITS, SOLVE_CEILING, validate};
use crate::filter::biquad::{Biquad, decay_ring, section_l1};
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
mod tests;
