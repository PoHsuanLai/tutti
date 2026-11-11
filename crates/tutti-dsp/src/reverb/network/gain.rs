//! The RT60 to loop-gain formula and its inverse.
//!
//! A signal that goes round a loop of `d` frames at gain `g` loses
//! `20 log10 g` dB per trip, and RT60 is the time to lose 60 dB, so
//! `g = 10^(-3 d / (RT60 fs))`. These two functions are that formula both
//! ways, in `f64` and through `libm` so a design does not depend on the
//! platform's C math library.

use tutti_types::{Amplitude, SampleRate, Seconds};

/// The loop gain that makes a loop of `delay` frames decay by 60 dB in `rt60`:
/// `g = 10^(-3 d / (RT60 fs))`.
///
/// `delay` is fractional frames (a bare `f32`: `Samples` is whole frames). The
/// gain is an `Amplitude`: a loop gain is non-negative. A
/// zero, negative or NaN `rt60` has no memory and gives gain 0; an infinite
/// one never decays and gives 1.
#[must_use]
pub fn loop_gain(delay: f32, rt60: Seconds, rate: SampleRate) -> Amplitude {
    let trips = rt60.get() * rate.get();
    if trips.is_nan() || trips <= 0.0 {
        return Amplitude::SILENT;
    }
    Amplitude(libm::exp10(-3.0 * f64::from(delay) / trips) as f32)
}

/// The inverse of [`loop_gain`]: the RT60 of a loop of `delay` frames at
/// gain `gain`, `-3 d / (fs log10 g)`.
///
/// A gain of 0 decays at once (zero seconds); a gain of 1 or more, and a gain
/// that is NaN or negative (not a loop gain), never settle: infinity.
#[must_use]
pub fn rt60_from_gain(gain: Amplitude, delay: f32, rate: SampleRate) -> Seconds {
    Seconds(decay_seconds(
        f64::from(delay),
        libm::log10(f64::from(gain.get())),
        rate.get(),
    ))
}

/// RT60 in seconds of a loop set (`delay_sum` frames in all, `log10_gain_sum`
/// the sum of the loop gains' base-10 logs): `-3 Σm / (fs Σ log10 g)`.
pub(crate) fn decay_seconds(delay_sum: f64, log10_gain_sum: f64, rate: f64) -> f64 {
    if log10_gain_sum.is_nan() || log10_gain_sum >= 0.0 {
        f64::INFINITY
    } else if log10_gain_sum == f64::NEG_INFINITY {
        0.0
    } else {
        -3.0 * delay_sum / (rate * log10_gain_sum)
    }
}

#[cfg(test)]
mod tests;
