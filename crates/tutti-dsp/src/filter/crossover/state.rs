//! The state of a Linkwitz-Riley crossover tree: [`CrossoverState`], the sections' words of one
//! channel, run with the coefficients of a [`CrossoverModel`].
//!
//! Each split is a low-pass and a high-pass of two cascaded 2nd-order Butterworth sections
//! (Q = 1/sqrt(2)) at the crossover frequency (Linkwitz and Riley, 1976). The two branches
//! are -6.02 dB and in phase at the crossover, so they sum to unity magnitude: the sum is the
//! 2nd-order Butterworth all-pass of that frequency.
//!
//! N bands are N-1 splits in a tree: split 0 divides the input into band 0 and the rest,
//! split 1 divides the rest into band 1 and the rest, and so on. A band split off before the
//! last crossover has not been through the later all-passes its siblings share, so it runs
//! through the all-pass of each later crossover. Every band then has the same phase
//! response: for a crossover with denominator `D`, `LP^2`, `HP^2` and the all-pass all have
//! phase `-2 * arg D`. The bands are phase-aligned at every frequency, so they sum without
//! cancelling, and the sum has flat magnitude.
//!
//! The filters are IIR, so the response is **not linear-phase**: the shared phase rotates
//! with frequency (the group delay is not constant), and the sum is an all-pass, not a
//! delayed copy of the input. They add no latency.

use super::CrossoverModel;
use super::bands::{MAX_BANDS, MAX_SPLITS};
use crate::filter::biquad::Biquad;

/// A biquad's two state words (transposed direct form II).
#[derive(Clone, Copy, Debug, Default)]
struct Section([f64; 2]);

impl Section {
    #[inline]
    fn tick(&mut self, c: &Biquad, x: f64) -> f64 {
        let y = c.b0 * x + self.0[0];
        self.0[0] = c.b1 * x - c.a1 * y + self.0[1];
        self.0[1] = c.b2 * x - c.a2 * y;
        y
    }
}

/// The state of one channel of a crossover tree: the sections' words, apart from the
/// model whose coefficients run them ([`CrossoverModel`]).
#[derive(Clone, Copy, Debug, Default)]
pub struct CrossoverState {
    /// Per split, the two cascaded sections of its low-pass and high-pass.
    low: [[Section; 2]; MAX_SPLITS],
    high: [[Section; 2]; MAX_SPLITS],
    /// `all_pass[j][k]`: band `j`'s all-pass for the later split `k > j`.
    all_pass: [[Section; MAX_SPLITS]; MAX_SPLITS],
}

impl CrossoverState {
    /// One sample through the tree `model` describes: its bands, lowest first, in
    /// the first entries of the result (the rest are 0).
    #[inline]
    pub(crate) fn tick(&mut self, model: &CrossoverModel, x: f32) -> [f32; MAX_BANDS] {
        let mut out = [0.0f32; MAX_BANDS];
        let splits = model.bands - 1;
        let mut rest = f64::from(x);
        for (j, slot) in out.iter_mut().enumerate().take(splits) {
            let c = &model.coeffs[j];
            let mut lo = rest;
            lo = self.low[j][0].tick(&c.low, lo);
            lo = self.low[j][1].tick(&c.low, lo);
            let mut hi = rest;
            hi = self.high[j][0].tick(&c.high, hi);
            hi = self.high[j][1].tick(&c.high, hi);
            for k in j + 1..splits {
                lo = self.all_pass[j][k].tick(&model.coeffs[k].all_pass, lo);
            }
            *slot = lo as f32;
            rest = hi;
        }
        out[splits] = rest as f32;
        out
    }
}

#[cfg(test)]
mod tests;
