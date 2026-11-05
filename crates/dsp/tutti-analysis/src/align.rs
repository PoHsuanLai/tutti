//! Take alignment and delay finding by GCC-PHAT.
//!
//! The generalised cross-correlation with the phase transform (Knapp and
//! Carter, 1976): the cross-spectrum `A * conj(B)` of the two takes is divided
//! by its own magnitude before the inverse transform, so every frequency counts
//! equally and only phase, which carries the delay, is left. Plain
//! cross-correlation weights bins by energy, so a loud hum or a strongly
//! coloured source smears or displaces the peak; PHAT leaves a sharp one.
//!
//! An offline or UI-thread API: it allocates the padded spectra and the
//! correlation on every call, through [`cross_correlate_shaped`].

use tutti_types::{Confidence, Hz, SampleRate, Samples, Seconds};

use crate::Complex;
use crate::error::{Error, Result};
use crate::fft::FftScratch;
use crate::xcorr::{CrossCorrelation, cross_correlate_shaped};

/// A bin whose cross-spectrum magnitude is below this fraction of the
/// strongest bin is damped rather than boosted to unit magnitude. Relative, so
/// the weighting does not depend on the inputs' level.
const PHAT_FLOOR: f32 = 1e-3;

/// What [`align`] searches and weights.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AlignConfig {
    max_lag: Samples,
    band: Option<Band>,
}

/// A pass band: the sample rate that maps bins to frequencies, and its edges.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Band {
    sample_rate: SampleRate,
    low: Hz,
    high: Hz,
}

impl AlignConfig {
    /// Search lags `-max_lag..=max_lag`, over the whole spectrum.
    #[must_use]
    pub fn new(max_lag: Samples) -> Self {
        Self {
            max_lag,
            band: None,
        }
    }

    /// Weight only bins between `low` and `high`, leaving the rest out of the
    /// correlation: a band where both takes carry the same source, away from
    /// rumble or hiss that does not.
    ///
    /// # Errors
    ///
    /// [`Error::NonPositiveSampleRate`] for a rate that is zero, negative or
    /// NaN; [`Error::EmptyFrequencyRange`] when `low` is negative or not below
    /// `high`; [`Error::AboveNyquist`] when `high` exceeds half the rate.
    pub fn with_band(
        mut self,
        sample_rate: impl Into<SampleRate>,
        low: impl Into<Hz>,
        high: impl Into<Hz>,
    ) -> Result<Self> {
        let (sample_rate, low, high) = (sample_rate.into(), low.into(), high.into());
        #[allow(
            clippy::neg_cmp_op_on_partial_ord,
            reason = "the negation is the NaN guard: `x <= 0.0` would admit a NaN rate"
        )]
        if !(sample_rate.get() > 0.0) {
            return Err(Error::NonPositiveSampleRate);
        }
        if !(low.get() >= 0.0 && low < high) {
            return Err(Error::EmptyFrequencyRange {
                min: low,
                max: high,
            });
        }
        let nyquist = Hz((sample_rate.get() / 2.0) as f32);
        if high > nyquist {
            return Err(Error::AboveNyquist {
                freq: high,
                nyquist,
            });
        }
        self.band = Some(Band {
            sample_rate,
            low,
            high,
        });
        Ok(self)
    }

    /// The search bound, in whole samples either side of zero.
    #[inline]
    #[must_use]
    pub fn max_lag(&self) -> Samples {
        self.max_lag
    }
}

/// Where a take sits against its reference.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Alignment {
    /// The whole-sample lag: positive means the take is a **delayed** copy of
    /// the reference, so shifting it earlier by this much aligns it.
    pub lag: isize,
    /// The sub-sample correction to `lag`, in samples, within `-0.5..=0.5`.
    /// A parabola through the peak and its two neighbours; zero when a
    /// neighbour is outside the overlap or the three are collinear.
    pub offset: f32,
    /// How distinct the peak is: `1 - second / peak`, where `second` is the
    /// strongest value outside the peak's own lobe. A reading of how clearly
    /// one lag wins, not a probability; near zero for unrelated signals.
    pub confidence: Confidence,
}

impl Alignment {
    /// The delay in samples, `lag + offset`.
    ///
    /// `f64` because the sum needs more digits than `f32` keeps for a long lag
    /// (an `f32` resolves a quarter sample only up to about 2^22).
    #[inline]
    #[must_use]
    pub fn delay_samples(&self) -> f64 {
        self.lag as f64 + f64::from(self.offset)
    }

    /// The delay in seconds at `sample_rate`.
    ///
    /// [`Seconds`] is `f32`: about seven significant digits, so a delay of a
    /// second resolves to roughly 0.1 microsecond, finer than the
    /// sub-sample estimate itself.
    #[inline]
    #[must_use]
    pub fn delay(&self, sample_rate: impl Into<SampleRate>) -> Seconds {
        Seconds((self.delay_samples() / sample_rate.into().get()) as f32)
    }
}

/// Finds how late `take` is against `reference`, by GCC-PHAT.
///
/// The sub-sample [`Alignment::offset`] is a parabolic fit: on a signal that
/// fills the band it is good to about a tenth of a sample, and it degrades as
/// the signal narrows or noise rises. The lag is searched over
/// `-max_lag..=max_lag`, clamped to the overlap.
///
/// # Errors
///
/// [`Error::InsufficientInput`] when either buffer is empty.
pub fn align(
    reference: &[f32],
    take: &[f32],
    config: &AlignConfig,
    fft: &mut FftScratch,
) -> Result<Alignment> {
    for buffer in [reference, take] {
        if buffer.is_empty() {
            return Err(Error::InsufficientInput {
                needed: Samples(1),
                got: Samples(0),
            });
        }
    }
    let correlation = cross_correlate_shaped(take, reference, fft, |bins| {
        phase_transform(bins, config.band);
    });
    // Lag zero is always in range for two non-empty buffers.
    let peak = correlation
        .peak_within(config.max_lag)
        .ok_or(Error::InsufficientInput {
            needed: Samples(1),
            got: Samples(0),
        })?;
    Ok(Alignment {
        lag: peak.lag,
        offset: parabolic_offset(&correlation, peak.lag),
        confidence: distinctness(&correlation, peak.lag, peak.value, config.max_lag),
    })
}

/// Divides each bin by its magnitude, and zeroes bins outside the band.
fn phase_transform(bins: &mut [Complex], band: Option<Band>) {
    let strongest = bins.iter().map(|b| b.norm()).fold(0.0f32, f32::max);
    let floor = strongest * PHAT_FLOOR;
    // `bins` holds `size / 2 + 1` bins of a `size`-point transform.
    let size = (bins.len().saturating_sub(1) * 2).max(1);
    for (k, bin) in bins.iter_mut().enumerate() {
        let in_band = band.is_none_or(|b| {
            let hz = k as f64 * b.sample_rate.get() / size as f64;
            hz >= f64::from(b.low.get()) && hz <= f64::from(b.high.get())
        });
        *bin = if in_band && strongest > 0.0 {
            *bin / (bin.norm() + floor)
        } else {
            Complex::default()
        };
    }
}

/// The parabola's vertex through `lag - 1`, `lag`, `lag + 1`, relative to
/// `lag`, clamped to half a sample.
fn parabolic_offset(correlation: &CrossCorrelation, lag: isize) -> f32 {
    let (Some(l), Some(c), Some(r)) = (
        correlation.at(lag - 1),
        correlation.at(lag),
        correlation.at(lag + 1),
    ) else {
        return 0.0;
    };
    let curvature = l - 2.0 * c + r;
    if curvature == 0.0 {
        return 0.0;
    }
    (0.5 * (l - r) / curvature).clamp(-0.5, 0.5)
}

/// `1 - second / peak` for the strongest value in the search window that is
/// not on the peak's own lobe (the run of falling values either side of it).
fn distinctness(
    correlation: &CrossCorrelation,
    lag: isize,
    peak: f32,
    max_lag: Samples,
) -> Confidence {
    if peak <= 0.0 {
        return Confidence::NONE;
    }
    let bound = isize::try_from(max_lag.get()).unwrap_or(isize::MAX);
    let (lo, hi) = (
        correlation.min_lag().max(-bound),
        correlation.max_lag().min(bound),
    );
    let value = |l: isize| correlation.at(l).unwrap_or(f32::NEG_INFINITY);
    // Walk out while the values fall; the lobe ends at the first rise.
    let mut left = lag;
    while left > lo && value(left - 1) < value(left) {
        left -= 1;
    }
    let mut right = lag;
    while right < hi && value(right + 1) < value(right) {
        right += 1;
    }
    let second = (lo..left)
        .chain(right + 1..=hi)
        .map(value)
        .fold(0.0f32, f32::max);
    Confidence::new_clamped(1.0 - second / peak)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cross_correlate;
    use rand::{Rng, SeedableRng, rngs::StdRng};

    fn noise(len: usize, seed: u64) -> Vec<f32> {
        let mut rng = StdRng::seed_from_u64(seed);
        (0..len).map(|_| rng.gen_range(-1.0..1.0)).collect()
    }

    /// `x` delayed by `delay` samples (negative: advanced), same length.
    fn shifted(x: &[f32], delay: isize) -> Vec<f32> {
        (0..x.len() as isize)
            .map(|n| {
                usize::try_from(n - delay)
                    .ok()
                    .and_then(|i| x.get(i))
                    .copied()
                    .unwrap_or(0.0)
            })
            .collect()
    }

    fn find(reference: &[f32], take: &[f32], max_lag: usize) -> Alignment {
        let cfg = AlignConfig::new(Samples(max_lag));
        align(reference, take, &cfg, &mut FftScratch::new()).unwrap()
    }

    /// A signal that fills the band, evaluable at fractional times: forty
    /// sines with seeded frequencies below 0.4 cycles per sample.
    fn sines(len: usize, delay: f32) -> Vec<f32> {
        let mut rng = StdRng::seed_from_u64(9);
        let partials: Vec<(f32, f32, f32)> = (0..40)
            .map(|_| {
                (
                    rng.gen_range(0.02..0.4),
                    rng.gen_range(0.0..core::f32::consts::TAU),
                    rng.gen_range(0.5..1.0),
                )
            })
            .collect();
        (0..len)
            .map(|n| {
                let t = n as f32 - delay;
                partials
                    .iter()
                    .map(|&(f, p, a)| a * (core::f32::consts::TAU * f * t + p).sin())
                    .sum()
            })
            .collect()
    }

    /// A sine of `amp` and a 480-sample period added to `x`, starting at `phase`.
    fn with_hum(mut x: Vec<f32>, amp: f32, phase: f32) -> Vec<f32> {
        for (n, v) in x.iter_mut().enumerate() {
            *v += amp * (core::f32::consts::TAU * n as f32 / 480.0 + phase).sin();
        }
        x
    }

    /// A take delayed by a whole number of samples is found at exactly that
    /// lag, either sign, short or long, with a distinct peak.
    ///
    /// Mutation checked: aligning `take` against `reference` the other way
    /// round (`align(take, reference)`) flips every lag's sign and fails.
    #[test]
    fn a_delayed_copy_is_found_at_exactly_its_delay() {
        let x = noise(4096, 1);
        for delay in [0isize, 1, -1, 123, -123, 1500, -1500] {
            let a = find(&x, &shifted(&x, delay), 2000);
            assert_eq!(a.lag, delay, "delay {delay}");
            assert!(a.offset.abs() < 0.05, "delay {delay}: offset {}", a.offset);
            assert!(
                a.confidence.get() > 0.8,
                "delay {delay}: {:?}",
                a.confidence
            );
        }
    }

    /// A band-limited signal delayed by a fraction of a sample is recovered
    /// to within a tenth of a sample by the parabolic refinement.
    ///
    /// Mutation checked: returning `offset = 0.0` leaves the half-sample
    /// delays 0.5 off and fails; negating the offset fails the 10.5 case.
    #[test]
    fn a_fractional_delay_is_recovered_within_a_tenth_of_a_sample() {
        for delay in [10.5f32, 3.25, -7.75, 100.5] {
            let a = find(&sines(4096, 0.0), &sines(4096, delay), 300);
            let err = (a.delay_samples() - f64::from(delay)).abs();
            assert!(err < 0.1, "delay {delay}: found {}", a.delay_samples());
        }
    }

    /// Scaling either input by a large or tiny gain changes neither lag nor
    /// confidence: the PHAT floor is relative to the spectrum's own level.
    ///
    /// Mutation checked: an absolute floor (`let floor = 1e-3`) damps every
    /// bin of the 1e-6 case to nothing and the lag is lost.
    #[test]
    fn scaling_an_input_changes_nothing() {
        let x = noise(4096, 1);
        let t = shifted(&x, 321);
        let base = find(&x, &t, 1000);
        assert_eq!(base.lag, 321);
        for (gr, gt) in [
            (1000.0f32, 1.0f32),
            (1.0, 1000.0),
            (1e-6, 1.0),
            (1.0, 1e-6),
            (1e-6, 1e-6),
        ] {
            let r: Vec<f32> = x.iter().map(|v| v * gr).collect();
            let k: Vec<f32> = t.iter().map(|v| v * gt).collect();
            let a = find(&r, &k, 1000);
            assert_eq!(a.lag, 321, "gains {gr}, {gt}");
            assert!(
                (a.confidence.get() - base.confidence.get()).abs() < 0.02,
                "gains {gr}, {gt}"
            );
        }
    }

    /// A loud interferer (hum with its own phase in each take) steals plain
    /// cross-correlation; PHAT weights every bin alike and keeps the delay.
    ///
    /// Mutation checked: making `phase_transform` leave the bins unnormalised
    /// (return before the loop) lands on the hum's lag and fails.
    #[test]
    fn phat_keeps_the_delay_where_plain_correlation_follows_a_hum() {
        let x = noise(8192, 2);
        let reference = with_hum(x.clone(), 3.0, 0.0);
        let take = with_hum(shifted(&x, 77), 3.0, 1.3);
        let plain = cross_correlate(&take, &reference, &mut FftScratch::new())
            .peak_within(Samples(1000))
            .unwrap();
        assert_ne!(
            plain.lag, 77,
            "the case no longer defeats plain correlation"
        );
        assert_eq!(find(&reference, &take, 1000).lag, 77);
    }

    /// A band that leaves out the hum recovers the delay where the full
    /// spectrum, swamped by the hum's leakage, does not.
    ///
    /// Mutation checked: treating every bin as in band (`in_band = true`)
    /// fails the banded assertion.
    #[test]
    fn a_band_limit_leaves_out_what_does_not_correlate() {
        let x = noise(8192, 2);
        let reference = with_hum(x.clone(), 50.0, 0.0);
        let take = with_hum(shifted(&x, 77), 50.0, 1.3);
        let mut fft = FftScratch::new();
        let open = AlignConfig::new(Samples(1000));
        assert_ne!(align(&reference, &take, &open, &mut fft).unwrap().lag, 77);
        let banded = open.with_band(48_000.0, 2000.0, 20_000.0).unwrap();
        assert_eq!(align(&reference, &take, &banded, &mut fft).unwrap().lag, 77);
    }

    /// A decaying, reverb-like filter on the take leaves the lag where it was.
    ///
    /// Mutation checked: negating the reported lag fails the pin. It is a
    /// robustness pin, not a discriminator: plain correlation also finds this
    /// lag, and the hum test is the one that tells PHAT from it.
    #[test]
    fn a_decaying_filter_on_the_take_keeps_the_lag() {
        let x = noise(8192, 4);
        let delayed = shifted(&x, 77);
        let taps: Vec<f32> = (0..64).map(|k| 0.9f32.powi(k)).collect();
        let take: Vec<f32> = (0..delayed.len())
            .map(|n| {
                taps.iter()
                    .enumerate()
                    .filter(|&(k, _)| k <= n)
                    .map(|(k, h)| h * delayed[n - k])
                    .sum()
            })
            .collect();
        let a = find(&x, &take, 1000);
        assert_eq!(a.lag, 77);
        assert!(a.confidence.get() > 0.5, "{:?}", a.confidence);
    }

    /// Only lags within the bound are chosen, the bound itself included.
    ///
    /// Mutation checked: searching the whole overlap (`peak()` in place of
    /// `peak_within`) returns 200 and fails; an exclusive bound fails the
    /// edge case.
    #[test]
    fn the_max_lag_bound_is_respected() {
        let x = noise(2048, 6);
        let take = shifted(&x, 200);
        assert_eq!(find(&x, &take, 200).lag, 200);
        let a = find(&x, &take, 150);
        assert!(a.lag.abs() <= 150, "lag {}", a.lag);
        assert_eq!(find(&x, &take, 0).lag, 0);
    }

    /// A peak that spans several lags (a narrow band) is not its own
    /// runner-up: the flanks of its lobe do not count as a second peak.
    ///
    /// Mutation checked: not walking out along the lobe (`while false && ...`
    /// on either walk) puts the flank in as the second peak, and the confidence
    /// drops from 0.45 to 0.05.
    #[test]
    fn a_wide_peak_is_not_its_own_runner_up() {
        let cfg = AlignConfig::new(Samples(300))
            .with_band(48_000.0, 1000.0, 4000.0)
            .unwrap();
        let a = align(
            &sines(4096, 0.0),
            &sines(4096, 40.0),
            &cfg,
            &mut FftScratch::new(),
        )
        .unwrap();
        assert_eq!(a.lag, 40);
        assert!(a.confidence.get() > 0.3, "{:?}", a.confidence);
    }

    /// Unrelated noise has no distinct peak.
    ///
    /// Mutation checked: reporting a constant `Confidence::CERTAIN` fails the
    /// bound.
    #[test]
    fn unrelated_noise_has_low_confidence() {
        for seed in 10..14 {
            let a = find(&noise(8192, seed), &noise(8192, seed + 100), 1000);
            assert!(a.confidence.get() < 0.3, "seed {seed}: {:?}", a.confidence);
        }
    }

    /// Delay in samples and seconds, and the input checks.
    ///
    /// Mutation checked: scaling the seconds by two fails the figure;
    /// replacing the empty-input check or the Nyquist check with `if false`
    /// fails the matching assertion.
    #[test]
    fn delay_converts_and_bad_input_is_refused() {
        let a = Alignment {
            lag: 48,
            offset: 0.25,
            confidence: Confidence::CERTAIN,
        };
        assert_eq!(a.delay_samples(), 48.25);
        assert!((a.delay(48_000.0).get() - 0.001_005_2).abs() < 1e-8);

        let mut fft = FftScratch::new();
        let cfg = AlignConfig::new(Samples(10));
        assert!(matches!(
            align(&[], &[1.0], &cfg, &mut fft),
            Err(Error::InsufficientInput { .. })
        ));
        assert!(matches!(
            align(&[1.0], &[], &cfg, &mut fft),
            Err(Error::InsufficientInput { .. })
        ));
        assert_eq!(
            cfg.with_band(48_000.0, 100.0, 30_000.0),
            Err(Error::AboveNyquist {
                freq: Hz(30_000.0),
                nyquist: Hz(24_000.0)
            })
        );
        assert!(matches!(
            cfg.with_band(48_000.0, 500.0, 100.0),
            Err(Error::EmptyFrequencyRange { .. })
        ));
        assert_eq!(
            cfg.with_band(0.0, 100.0, 200.0),
            Err(Error::NonPositiveSampleRate)
        );
    }
}
