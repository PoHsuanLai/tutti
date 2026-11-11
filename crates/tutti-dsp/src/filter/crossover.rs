//! Linkwitz-Riley 4th-order crossover: [`Crossover`].
//!
//! Each split is a low-pass and a high-pass of two cascaded 2nd-order
//! Butterworth sections (Q = 1/sqrt(2)) at the crossover frequency
//! (Linkwitz and Riley, 1976). The two branches are -6.02 dB and in phase at
//! the crossover, so they sum to unity magnitude: the sum is the 2nd-order
//! Butterworth all-pass of that frequency.
//!
//! N bands are N-1 splits in a tree: split 0 divides the input into band 0
//! and the rest, split 1 divides the rest into band 1 and the rest, and so
//! on. A band split off before the last crossover has not been through the
//! later all-passes its siblings share, so it runs through the all-pass of
//! each later crossover. Every band then has the same phase response: for a
//! crossover with denominator `D`, `LP^2`, `HP^2` and the all-pass all have
//! phase `-2 * arg D`. The bands are phase-aligned at every frequency, so
//! they sum without cancelling, and the sum has flat magnitude.
//!
//! The filters are IIR, so the response is **not linear-phase**: the shared
//! phase rotates with frequency (the group delay is not constant), and
//! the sum is an all-pass, not a delayed copy of the input. They add no
//! latency.

use std::fmt;

use tutti_types::{Hz, SampleRate};

use super::biquad::Biquad;
use super::crossover_model::CrossoverModel;
use crate::COEFF_INTERVAL;

/// Most splits a [`Crossover`] holds: one fewer than [`Crossover::MAX_BANDS`].
pub const MAX_SPLITS: usize = 5;

/// Highest frequency the coefficients are solved at, as a fraction of the
/// sample rate. `tan` diverges at Nyquist; a frequency a rate change leaves
/// above this is solved here instead.
pub const SOLVE_CEILING: f64 = 0.499;

/// Why a [`Crossover`] or crossover-based node refused its settings.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum CrossoverError {
    /// The band count is outside the supported range.
    Bands {
        /// The count asked for.
        bands: usize,
        /// The fewest supported.
        min: usize,
        /// The most supported.
        max: usize,
    },
    /// A whole set of frequencies has the wrong length.
    Count {
        /// How many were given.
        got: usize,
        /// How many there are crossovers.
        expected: usize,
    },
    /// The channel count is zero, or more channels than the graph has ports
    /// for.
    Channels {
        /// The count asked for.
        channels: usize,
    },
    /// The sample rate is not finite and positive.
    SampleRate,
    /// A frequency is not finite, or not above 0 Hz.
    Frequency {
        /// Which crossover.
        index: usize,
        /// The value given.
        hz: Hz,
    },
    /// A frequency is not above the one before it.
    Order {
        /// Which crossover.
        index: usize,
    },
    /// A frequency is not below Nyquist.
    Nyquist {
        /// Which crossover.
        index: usize,
        /// Nyquist at the rate given.
        nyquist: Hz,
    },
    /// No crossover has this index.
    Index {
        /// The index given.
        index: usize,
        /// How many crossovers there are.
        len: usize,
    },
}

impl fmt::Display for CrossoverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Bands { bands, min, max } => {
                write!(f, "{bands} bands: {min} to {max} are supported")
            }
            Self::Count { got, expected } => {
                write!(f, "{got} frequencies given for {expected} crossovers")
            }
            Self::Channels { channels } => write!(f, "{channels} channels is not supported"),
            Self::SampleRate => f.write_str("the sample rate must be finite and positive"),
            Self::Frequency { index, hz } => write!(
                f,
                "crossover {index} is {} Hz: it must be finite and above 0",
                hz.get()
            ),
            Self::Order { index } => write!(
                f,
                "crossover {index} is not above crossover {}: they must strictly increase",
                index.saturating_sub(1)
            ),
            Self::Nyquist { index, nyquist } => write!(
                f,
                "crossover {index} is not below Nyquist ({} Hz)",
                nyquist.get()
            ),
            Self::Index { index, len } => {
                write!(f, "no crossover {index}: there are {len}")
            }
        }
    }
}

impl std::error::Error for CrossoverError {}

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
    pub(super) fn tick(&mut self, model: &CrossoverModel, x: f32) -> [f32; Crossover::MAX_BANDS] {
        let mut out = [0.0f32; Crossover::MAX_BANDS];
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

/// A Linkwitz-Riley 4th-order crossover of 2 to 6 bands over one channel.
///
/// State and coefficients are `f64`; samples in and out are `f32`. A
/// crossover frequency change ([`set_frequency`](Self::set_frequency)) is
/// ramped across the next [`split_block`](Self::split_block), with the
/// coefficients re-solved every few samples from the interpolated frequency,
/// so it does not click.
///
/// A multichannel signal needs one `Crossover` per channel, all set to the
/// same frequencies.
///
/// ```
/// use tutti_dsp::Crossover;
/// use tutti_types::Hz;
///
/// let mut xo = Crossover::new(&[Hz(200.0), Hz(2_000.0)], tutti_types::SampleRate::new(48_000.0)).unwrap();
/// assert_eq!(xo.bands(), 3);
/// // The sum of the bands is an all-pass of the input: a constant 1.0
/// // settles to a sum of 1.0 (unity gain at DC).
/// let mut sum = 0.0;
/// for _ in 0..48_000 {
///     sum = xo.split(1.0)[..3].iter().sum::<f32>();
/// }
/// assert!((sum - 1.0).abs() < 1e-5);
/// ```
#[derive(Clone, Debug)]
pub struct Crossover {
    /// The design: the rate and the frequencies the coefficients are solved at
    /// now, and those coefficients. Everything else here is state.
    model: CrossoverModel,
    /// The frequencies set; the model's ramp to them.
    target: [f64; MAX_SPLITS],
    state: CrossoverState,
}

impl Crossover {
    /// The fewest bands.
    pub const MIN_BANDS: usize = 2;
    /// The most bands.
    pub const MAX_BANDS: usize = MAX_SPLITS + 1;

    /// A crossover with one split at each of `frequencies`, so
    /// `frequencies.len() + 1` bands, at `rate`.
    ///
    /// # Errors
    ///
    /// [`CrossoverError`] if the band count is not in
    /// [`MIN_BANDS`](Self::MIN_BANDS)`..=`[`MAX_BANDS`](Self::MAX_BANDS), the
    /// rate is not finite and positive, or the frequencies are not finite,
    /// above 0, strictly increasing and below Nyquist.
    pub fn new(frequencies: &[Hz], rate: impl Into<SampleRate>) -> Result<Self, CrossoverError> {
        let rate = rate.into();
        validate(frequencies, rate)?;
        let mut xo = Self {
            model: CrossoverModel::unsolved(frequencies.len() + 1, rate.get()),
            target: [0.0; MAX_SPLITS],
            state: CrossoverState::default(),
        };
        for (j, f) in frequencies.iter().enumerate() {
            xo.target[j] = f64::from(f.get());
        }
        xo.settle();
        Ok(xo)
    }

    /// How many bands [`split`](Self::split) produces.
    #[must_use]
    pub fn bands(&self) -> usize {
        self.model.bands
    }

    /// Crossover `i`'s frequency as last set (the ramp's target), or `None`
    /// past the last.
    #[must_use]
    pub fn frequency(&self, i: usize) -> Option<Hz> {
        (i < self.splits()).then(|| Hz(self.target[i] as f32))
    }

    /// The design at the frequencies the coefficients are solved at now (a ramp
    /// in flight is between two of these): the splits' coefficients without the
    /// state that runs them.
    #[must_use]
    pub fn model(&self) -> &CrossoverModel {
        &self.model
    }

    /// The sample rate the coefficients are solved at.
    #[must_use]
    pub fn sample_rate(&self) -> SampleRate {
        SampleRate(self.model.rate)
    }

    fn splits(&self) -> usize {
        self.model.bands - 1
    }

    /// Moves crossover `i` to `hz`: ramped across the next
    /// [`split_block`](Self::split_block), or taken whole by the next
    /// [`split`](Self::split).
    ///
    /// # Errors
    ///
    /// [`CrossoverError`] if `i` is past the last crossover, or `hz` is not
    /// finite, above 0, above the crossover before it, below the one after
    /// it and below Nyquist. Nothing changes then.
    pub fn set_frequency(&mut self, i: usize, hz: Hz) -> Result<(), CrossoverError> {
        let n = self.splits();
        if i >= n {
            return Err(CrossoverError::Index { index: i, len: n });
        }
        check_one(i, hz, &self.target[..n], SampleRate(self.model.rate))?;
        self.target[i] = f64::from(hz.get());
        Ok(())
    }

    /// Moves every crossover at once, as [`set_frequency`](Self::set_frequency)
    /// moves one. `frequencies` is all of them: its length must be
    /// [`bands`](Self::bands) `- 1`.
    ///
    /// # Errors
    ///
    /// [`CrossoverError`] if the length differs, or the frequencies are not
    /// finite, above 0, strictly increasing and below Nyquist. Nothing
    /// changes then.
    pub fn set_frequencies(&mut self, frequencies: &[Hz]) -> Result<(), CrossoverError> {
        if frequencies.len() != self.splits() {
            return Err(CrossoverError::Count {
                got: frequencies.len(),
                expected: self.splits(),
            });
        }
        validate(frequencies, SampleRate(self.model.rate))?;
        for (t, f) in self.target.iter_mut().zip(frequencies) {
            *t = f64::from(f.get());
        }
        Ok(())
    }

    /// Re-solves for `rate`, keeping the filter state and the frequencies. A
    /// rate that is not finite and positive changes nothing. A frequency at
    /// or above the new Nyquist is solved just under it.
    pub fn set_sample_rate(&mut self, rate: impl Into<SampleRate>) {
        let rate = rate.into().get();
        if rate.is_finite() && rate > 0.0 {
            self.model.rate = rate;
            self.settle();
        }
    }

    /// Clears the filter state, and takes any pending frequency change whole.
    pub fn reset(&mut self) {
        self.state = CrossoverState::default();
        self.settle();
    }

    /// Splits one sample into [`bands`](Self::bands) bands, in the first
    /// entries of the result (the rest are 0), lowest band first. A pending
    /// frequency change lands whole on this sample: for a ramp, use
    /// [`split_block`](Self::split_block).
    #[inline]
    pub fn split(&mut self, x: f32) -> [f32; Self::MAX_BANDS] {
        if self.model.freqs != self.target {
            self.settle();
        }
        self.tick(x)
    }

    /// Splits `input` into [`bands`](Self::bands) bands: band `b` is written
    /// to `out[b * input.len()..][..input.len()]`. A pending frequency change
    /// ramps across the block, the last frame landing exactly on it.
    ///
    /// # Panics
    ///
    /// If `out.len()` is not `bands() * input.len()`.
    pub fn split_block(&mut self, input: &[f32], out: &mut [f32]) {
        let n = input.len();
        assert_eq!(
            out.len(),
            self.model.bands * n,
            "split_block: out holds one block per band"
        );
        let from = self.model.freqs;
        let mut done = 0;
        while done < n {
            let len = if self.model.freqs == self.target {
                n - done
            } else {
                COEFF_INTERVAL.min(n - done)
            };
            if self.model.freqs != self.target {
                let end = done + len;
                let splits = self.splits();
                let ramps = self.model.freqs.iter_mut().zip(&self.target).zip(&from);
                for ((cur, &to), &from) in ramps.take(splits) {
                    *cur = if end >= n {
                        to
                    } else {
                        from + (to - from) * (end as f64 / n as f64)
                    };
                }
                self.solve();
            }
            for i in done..done + len {
                let bands = self.tick(input[i]);
                for (b, y) in bands.iter().take(self.model.bands).enumerate() {
                    out[b * n + i] = *y;
                }
            }
            done += len;
        }
    }

    /// Takes the targets whole.
    fn settle(&mut self) {
        self.model.freqs = self.target;
        self.solve();
    }

    fn solve(&mut self) {
        self.model.solve();
    }

    #[inline]
    fn tick(&mut self, x: f32) -> [f32; Self::MAX_BANDS] {
        self.state.tick(&self.model, x)
    }
}

/// Checks the band count against what a crossover supports.
pub(super) fn check_bands(bands: usize) -> Result<(), CrossoverError> {
    if (Crossover::MIN_BANDS..=Crossover::MAX_BANDS).contains(&bands) {
        Ok(())
    } else {
        Err(CrossoverError::Bands {
            bands,
            min: Crossover::MIN_BANDS,
            max: Crossover::MAX_BANDS,
        })
    }
}

/// Checks a whole set: count, then each frequency in order.
pub(super) fn validate(frequencies: &[Hz], rate: SampleRate) -> Result<(), CrossoverError> {
    check_bands(frequencies.len() + 1)?;
    if !(rate.get().is_finite() && rate.get() > 0.0) {
        return Err(CrossoverError::SampleRate);
    }
    let nyquist = rate.nyquist();
    for (index, &hz) in frequencies.iter().enumerate() {
        if !(hz.get().is_finite() && hz.get() > 0.0) {
            return Err(CrossoverError::Frequency { index, hz });
        }
        if index > 0 && hz <= frequencies[index - 1] {
            return Err(CrossoverError::Order { index });
        }
        if hz >= nyquist {
            return Err(CrossoverError::Nyquist { index, nyquist });
        }
    }
    Ok(())
}

/// Checks `hz` as crossover `i` against the others' `targets`, and Nyquist.
fn check_one(i: usize, hz: Hz, targets: &[f64], rate: SampleRate) -> Result<(), CrossoverError> {
    // Every target came from an `Hz`, so narrowing it back is exact.
    check_neighbours(i, hz, |k| Hz(targets[k] as f32), targets.len())?;
    let nyquist = rate.nyquist();
    if hz >= nyquist {
        return Err(CrossoverError::Nyquist { index: i, nyquist });
    }
    Ok(())
}

/// Clamps each of `frequencies` under what the coefficients can be solved at
/// for `rate`, keeping them strictly increasing: index `i` of `n` is capped at
/// `SOLVE_CEILING * rate * (1 - 0.01 * (n - 1 - i))`, a strictly increasing
/// cap, so the minimum of it and a strictly increasing set still is. A
/// frequency out of range costs only itself, not the whole set.
pub fn fit_to_rate(frequencies: &mut [Hz], rate: SampleRate) {
    let n = frequencies.len();
    for (i, f) in frequencies.iter_mut().enumerate() {
        let cap = SOLVE_CEILING * rate.get() * (1.0 - 0.01 * (n - 1 - i) as f64);
        *f = Hz(f64::from(f.get()).min(cap) as f32);
    }
}

/// Finite, above 0, and between the neighbours `at(i - 1)` and `at(i + 1)`.
fn check_neighbours(
    i: usize,
    hz: Hz,
    at: impl Fn(usize) -> Hz,
    len: usize,
) -> Result<(), CrossoverError> {
    if !(hz.get().is_finite() && hz.get() > 0.0) {
        return Err(CrossoverError::Frequency { index: i, hz });
    }
    if i > 0 && hz <= at(i - 1) {
        return Err(CrossoverError::Order { index: i });
    }
    if i + 1 < len && hz >= at(i + 1) {
        return Err(CrossoverError::Order { index: i + 1 });
    }
    Ok(())
}

#[cfg(test)]
mod tests;
