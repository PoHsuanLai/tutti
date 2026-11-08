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
    let trips = f64::from(rt60.get()) * rate.get();
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
    ) as f32)
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
mod tests {
    use super::*;

    const RATE: SampleRate = SampleRate(48_000.0);

    /// A 1 s loop (48 000 frames) at RT60 1 s loses 60 dB per trip: the gain is
    /// 10^-3 exactly. At RT60 2 s it loses 30 dB: 10^-1.5.
    ///
    /// Mutation (run): the `3.0` replaced by `6.0` in `loop_gain` fails both.
    #[test]
    fn a_loop_that_decays_sixty_decibels_per_trip_has_a_thousandth_gain() {
        assert!((loop_gain(48_000.0, Seconds(1.0), RATE).get() - 1e-3).abs() < 1e-9);
        let g = loop_gain(48_000.0, Seconds(2.0), RATE);
        assert!((g.get() - 10f32.powf(-1.5)).abs() < 1e-6, "{g}");
    }

    /// The two functions are inverses, over delays, decays and rates.
    ///
    /// Mutation (run): a `+` for the `-` in `decay_seconds` fails the round
    /// trip (negative RT60).
    #[test]
    fn rt60_from_gain_inverts_loop_gain() {
        for rate in [44_100.0, 48_000.0, 96_000.0] {
            for delay in [1.0f32, 37.5, 1116.0, 40_000.0] {
                for rt60 in [0.05f32, 0.5, 2.0, 8.0] {
                    let g = loop_gain(delay, Seconds(rt60), SampleRate(rate));
                    if g.get() <= 1e-6 {
                        continue; // below f32's resolution of a gain
                    }
                    let back = rt60_from_gain(g, delay, SampleRate(rate)).get();
                    assert!((back - rt60).abs() <= 2e-3 * rt60, "{back} vs {rt60}");
                }
            }
        }
    }

    /// The edges: no memory is gain 0, never decaying is gain 1 and the inverse
    /// reads those back, a gain above 1 or NaN is unbounded, a zero delay at
    /// gain below 1 decays at once.
    ///
    /// Mutation (run): returning 1 for `trips <= 0.0` fails the first assert.
    #[test]
    fn the_edges_of_the_formula() {
        assert_eq!(loop_gain(100.0, Seconds(0.0), RATE), Amplitude::SILENT);
        assert_eq!(loop_gain(100.0, Seconds(f32::NAN), RATE), Amplitude::SILENT);
        assert_eq!(
            loop_gain(100.0, Seconds(f32::INFINITY), RATE),
            Amplitude::UNITY
        );
        assert_eq!(rt60_from_gain(Amplitude(0.0), 100.0, RATE).get(), 0.0);
        assert_eq!(
            rt60_from_gain(Amplitude(1.0), 100.0, RATE).get(),
            f32::INFINITY
        );
        assert_eq!(
            rt60_from_gain(Amplitude(1.2), 100.0, RATE).get(),
            f32::INFINITY
        );
        assert_eq!(
            rt60_from_gain(Amplitude(f32::NAN), 100.0, RATE).get(),
            f32::INFINITY
        );
        assert_eq!(
            rt60_from_gain(Amplitude(-0.5), 100.0, RATE).get(),
            f32::INFINITY
        );
        assert_eq!(rt60_from_gain(Amplitude(0.5), 0.0, RATE).get(), 0.0);
    }
}
