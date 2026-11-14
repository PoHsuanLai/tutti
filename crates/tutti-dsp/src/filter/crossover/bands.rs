//! What a Linkwitz-Riley crossover's design refuses and the limits it solves within:
//! [`CrossoverError`], the band count and the frequency checks, and the fit of a set
//! under the rate.

use tutti_types::{Hz, SampleRate};

/// Most splits a crossover holds: one fewer than the most bands.
pub const MAX_SPLITS: usize = 5;

/// The fewest bands.
pub(super) const MIN_BANDS: usize = 2;

/// The most bands.
pub(super) const MAX_BANDS: usize = MAX_SPLITS + 1;

/// Highest frequency the coefficients are solved at, as a fraction of the
/// sample rate. `tan` diverges at Nyquist; a frequency a rate change leaves
/// above this is solved here instead.
pub const SOLVE_CEILING: f64 = 0.499;

/// Why a crossover or a crossover-based node refused its settings.
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum CrossoverError {
    /// The band count is outside the supported range.
    #[error("{bands} bands: {min} to {max} are supported")]
    Bands {
        /// The count asked for.
        bands: usize,
        /// The fewest supported.
        min: usize,
        /// The most supported.
        max: usize,
    },
    /// The channel count is zero, or more channels than the graph has ports
    /// for.
    #[error("{channels} channels is not supported")]
    Channels {
        /// The count asked for.
        channels: usize,
    },
    /// A frequency is not finite, or not above 0 Hz.
    #[error("crossover {index} is {} Hz: it must be finite and above 0", hz.get())]
    Frequency {
        /// Which crossover.
        index: usize,
        /// The value given.
        hz: Hz,
    },
    /// A frequency is not above the one before it.
    #[error("crossover {index} is not above crossover {}: they must strictly increase", index.saturating_sub(1))]
    Order {
        /// Which crossover.
        index: usize,
    },
    /// A frequency is not below Nyquist.
    #[error("crossover {index} is not below Nyquist ({} Hz)", nyquist.get())]
    Nyquist {
        /// Which crossover.
        index: usize,
        /// Nyquist at the rate given.
        nyquist: Hz,
    },
}

/// Checks the band count against what a crossover supports.
pub(super) fn check_bands(bands: usize) -> Result<(), CrossoverError> {
    if (MIN_BANDS..=MAX_BANDS).contains(&bands) {
        Ok(())
    } else {
        Err(CrossoverError::Bands {
            bands,
            min: MIN_BANDS,
            max: MAX_BANDS,
        })
    }
}

/// Checks a set against the rate-free limits: the count, then each frequency in order
/// (finite, above 0, above the one before).
pub(super) fn validate_order(frequencies: &[Hz]) -> Result<(), CrossoverError> {
    check_bands(frequencies.len() + 1)?;
    for (index, &hz) in frequencies.iter().enumerate() {
        if !(hz.get().is_finite() && hz.get() > 0.0) {
            return Err(CrossoverError::Frequency { index, hz });
        }
        if index > 0 && hz <= frequencies[index - 1] {
            return Err(CrossoverError::Order { index });
        }
    }
    Ok(())
}

/// Checks a whole set at `rate`: [`validate_order`], then each frequency below Nyquist.
pub(super) fn validate(frequencies: &[Hz], rate: SampleRate) -> Result<(), CrossoverError> {
    validate_order(frequencies)?;
    let nyquist = rate.nyquist();
    for (index, &hz) in frequencies.iter().enumerate() {
        if hz >= nyquist {
            return Err(CrossoverError::Nyquist { index, nyquist });
        }
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

#[cfg(test)]
mod tests;
