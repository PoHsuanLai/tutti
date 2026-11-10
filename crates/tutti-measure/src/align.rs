//! Take alignment and delay finding by GCC-PHAT.
//!
//! The generalised cross-correlation with the phase transform (Knapp and
//! Carter, 1976): the cross-spectrum `A * conj(B)` of the two takes is divided
//! by its own magnitude before the inverse transform, so every frequency counts
//! equally and only phase, which carries the delay, is left. Plain
//! cross-correlation weights bins by energy, so a loud hum or a strongly
//! coloured source smears or displaces the peak; PHAT leaves a sharp one.
//! The weighting whitens across about 40 dB of per-input dynamic range below
//! the strongest in-band bin; bins under that are damped, not boosted. A hum
//! stronger than that over the rest of the spectrum belongs outside a band.
//!
//! An offline or UI-thread API: it allocates the padded spectra and the
//! correlation on every call, through [`cross_correlate_shaped`].

use tutti_types::{Confidence, Hz, SampleRate, Samples, Seconds};

use crate::xcorr::{CrossCorrelation, cross_correlate_shaped};
use tutti_dsp::Complex;
use tutti_dsp::FftScratch;
use tutti_dsp::{Error, Result};

/// A bin whose cross-spectrum magnitude is below this fraction of the
/// strongest in-band bin is damped rather than boosted to unit magnitude.
/// Relative, so the weighting does not depend on the inputs' level. The
/// magnitude is a product of two amplitudes, so this is a ratio of
/// `sqrt(1e-4)`, 40 dB, per input. Lower, and the spectral leakage of a
/// truncated sinusoid (about 50 dB down) is whitened into the result and
/// biases the sub-sample lag; higher, and a hum 40 dB over the rest reduces
/// the weighting to plain correlation.
const PHAT_FLOOR: f32 = 1e-4;

/// The share of the peak below which the peak's lobe is taken to have ended.
const LOBE_FRACTION: f32 = 0.5;

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

    /// The pass band, as `(low, high)`, or `None` for the whole spectrum.
    #[inline]
    #[must_use]
    pub fn band(&self) -> Option<(Hz, Hz)> {
        self.band.map(|b| (b.low, b.high))
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
    /// neighbour is outside the overlap or the three do not bend down.
    ///
    /// A bare `f32`: [`Samples`] is a whole-sample count, and a fraction of a
    /// sample has no unit type of its own.
    pub offset: f32,
    /// How distinct the peak is: `1 - second / peak`, where `second` is the
    /// strongest value in the search window outside the peak's own lobe (the
    /// run around it that stays above half its height, down to the next local
    /// minimum). A reading of how clearly one lag wins, not a probability:
    /// near one for a sharp peak over a quiet floor, near zero for unrelated
    /// signals, and zero when the window holds no competitor (a bound of
    /// zero, or a lobe that fills it).
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
    /// [`Seconds`] is `f64`, finer than the sub-sample estimate itself.
    #[inline]
    #[must_use]
    pub fn delay(&self, sample_rate: impl Into<SampleRate>) -> Seconds {
        Seconds(self.delay_samples() / sample_rate.into().get())
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
/// [`Error::InsufficientInput`] when either buffer is empty;
/// [`Error::NonFiniteInput`] when a sample is NaN or infinite;
/// [`Error::NoSignal`] when there is nothing to correlate: a silent input, or
/// a band that holds no spectrum bin.
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
        if !buffer.iter().all(|v| v.is_finite()) {
            return Err(Error::NonFiniteInput);
        }
    }
    let mut signal = false;
    let correlation = cross_correlate_shaped(take, reference, fft, |bins| {
        signal = phase_transform(bins, config.band);
    });
    if !signal {
        return Err(Error::NoSignal);
    }
    // Two non-empty buffers always overlap at lag zero, so the window is never
    // empty; the error is the type's way of saying so.
    let peak = correlation
        .peak_within(config.max_lag)
        .ok_or(Error::NoSignal)?;
    Ok(Alignment {
        lag: peak.lag,
        offset: parabolic_offset(&correlation, peak.lag),
        confidence: distinctness(
            |l| correlation.at(l).unwrap_or(f32::NEG_INFINITY),
            peak.lag,
            peak.value,
            window(&correlation, config.max_lag),
        ),
    })
}

/// The lags `peak_within(max_lag)` searches: `-max_lag..=max_lag` clamped to the
/// overlap.
fn window(correlation: &CrossCorrelation, max_lag: Samples) -> (isize, isize) {
    let bound = isize::try_from(max_lag.get()).unwrap_or(isize::MAX);
    (
        correlation.min_lag().max(-bound),
        correlation.max_lag().min(bound),
    )
}

/// Whitens the in-band bins and zeroes the rest; `false` when no in-band bin
/// carries energy.
///
/// Each in-band bin is divided by `|bin| + floor`, `floor` being
/// [`PHAT_FLOOR`] of the strongest **in-band** bin: out-of-band energy (a hum
/// below the band) does not set the level. Bins more than `PHAT_FLOOR` below
/// the strongest are damped rather than boosted, so the whitening spans the
/// source's dynamic range down to that level.
fn phase_transform(bins: &mut [Complex], band: Option<Band>) -> bool {
    // `bins` holds `size / 2 + 1` bins of a `size`-point transform.
    let size = (bins.len().saturating_sub(1) * 2).max(1);
    let in_band = |k: usize| {
        band.is_none_or(|b| {
            let hz = k as f64 * b.sample_rate.get() / size as f64;
            hz >= f64::from(b.low.get()) && hz <= f64::from(b.high.get())
        })
    };
    let strongest = bins
        .iter()
        .enumerate()
        .filter(|&(k, _)| in_band(k))
        .map(|(_, b)| b.norm())
        .fold(0.0f32, f32::max);
    if strongest <= 0.0 {
        bins.fill(Complex::default());
        return false;
    }
    let floor = strongest * PHAT_FLOOR;
    for (k, bin) in bins.iter_mut().enumerate() {
        *bin = if in_band(k) {
            *bin / (bin.norm() + floor)
        } else {
            Complex::default()
        };
    }
    true
}

/// The parabola's vertex through `lag - 1`, `lag`, `lag + 1`, relative to
/// `lag`, clamped to half a sample. Zero unless the three points bend down
/// (`curvature < 0`, which also rejects NaN).
fn parabolic_offset(correlation: &CrossCorrelation, lag: isize) -> f32 {
    let (Some(l), Some(c), Some(r)) = (
        correlation.at(lag - 1),
        correlation.at(lag),
        correlation.at(lag + 1),
    ) else {
        return 0.0;
    };
    let curvature = l - 2.0 * c + r;
    if curvature < 0.0 {
        (0.5 * (l - r) / curvature).clamp(-0.5, 0.5)
    } else {
        0.0
    }
}

/// `1 - second / peak` for the strongest value in `lo..=hi` outside the peak's
/// own lobe; `NONE` when the window has no such competitor.
///
/// The lobe runs outward from `lag` while values stay at or above
/// [`LOBE_FRACTION`] of the peak (so ripple or an exact tie at the top stays
/// inside), then on down to the next local minimum.
fn distinctness(
    value: impl Fn(isize) -> f32,
    lag: isize,
    peak: f32,
    (lo, hi): (isize, isize),
) -> Confidence {
    if peak <= 0.0 {
        return Confidence::NONE;
    }
    let edge = |step: isize, limit: isize| {
        let mut at = lag;
        while at != limit && value(at + step) >= LOBE_FRACTION * peak {
            at += step;
        }
        while at != limit && value(at + step) < value(at) {
            at += step;
        }
        at
    };
    let (left, right) = (edge(-1, lo), edge(1, hi));
    (lo..left)
        .chain(right + 1..=hi)
        .map(value)
        .reduce(f32::max)
        .map_or(Confidence::NONE, |second| {
            Confidence::new_clamped(1.0 - second.max(0.0) / peak)
        })
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

    /// The lag plain cross-correlation picks for the same pair.
    fn plain_lag(reference: &[f32], take: &[f32], max_lag: usize) -> isize {
        cross_correlate(take, reference, &mut FftScratch::new())
            .peak_within(Samples(max_lag))
            .unwrap()
            .lag
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

    /// A sine of `amp` and the given period, in samples, added to `x`.
    fn with_tone(mut x: Vec<f32>, amp: f32, period: f32, phase: f32) -> Vec<f32> {
        for (n, v) in x.iter_mut().enumerate() {
            *v += amp * (core::f32::consts::TAU * n as f32 / period + phase).sin();
        }
        x
    }

    /// White noise with everything outside `low..high` (cycles per sample)
    /// removed in the frequency domain.
    fn band_noise(len: usize, seed: u64, low: f32, high: f32) -> Vec<f32> {
        let mut fft = FftScratch::new();
        let mut bins = vec![Complex::default(); len / 2 + 1];
        fft.forward(&noise(len, seed), &vec![1.0; len], &mut bins);
        for (k, b) in bins.iter_mut().enumerate() {
            let f = k as f32 / len as f32;
            if f < low || f > high {
                *b = Complex::default();
            }
        }
        let mut out = vec![0.0; len];
        fft.inverse(&bins, &mut out);
        out
    }

    /// Noise through two one-pole lowpasses: a spectrum falling 12 dB per
    /// octave, some 70 dB from the lowest bins to the highest.
    fn tilted_noise(len: usize, seed: u64) -> Vec<f32> {
        let mut rng = StdRng::seed_from_u64(seed);
        let (mut a, mut b) = (0.0f32, 0.0f32);
        (0..len)
            .map(|_| {
                a = 0.95 * a + rng.gen_range(-1.0..1.0);
                b = 0.95 * b + a;
                b
            })
            .collect()
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
    /// The inputs carry a hum, so that a weighting that falls back to energy
    /// at low level (plain correlation) would follow the hum, not the delay.
    ///
    /// Mutation checked: an absolute floor (`let floor = PHAT_FLOOR;`) damps
    /// every bin of the 1e-6 cases and the lag follows the hum, so it fails.
    #[test]
    fn scaling_an_input_changes_nothing() {
        let x = noise(8192, 2);
        let reference = with_tone(x.clone(), 3.0, 480.0, 0.0);
        let take = with_tone(shifted(&x, 77), 3.0, 480.0, 1.3);
        let base = find(&reference, &take, 1000);
        assert_eq!(base.lag, 77);
        let scaled = |v: &[f32], g: f32| v.iter().map(|s| s * g).collect::<Vec<f32>>();
        for (gr, gt) in [
            (1000.0f32, 1.0f32),
            (1.0, 1000.0),
            (1e-6, 1.0),
            (1.0, 1e-6),
            (1e-6, 1e-6),
        ] {
            let a = find(&scaled(&reference, gr), &scaled(&take, gt), 1000);
            assert_eq!(a.lag, 77, "gains {gr}, {gt}");
            let drift = (a.confidence.get() - base.confidence.get()).abs();
            assert!(drift < 0.02, "gains {gr}, {gt}: drift {drift}");
        }
    }

    /// A loud interferer (a tone with its own phase in each take) steals plain
    /// cross-correlation; PHAT weights every bin alike and keeps the delay.
    ///
    /// Mutation checked: making `phase_transform` leave the bins unnormalised
    /// (`*bin / (bin.norm() + floor)` as `*bin`) lands on the tone's lag and
    /// fails.
    #[test]
    fn phat_keeps_the_delay_where_plain_correlation_follows_a_hum() {
        let x = noise(8192, 2);
        let reference = with_tone(x.clone(), 10.0, 480.0, 0.0);
        let take = with_tone(shifted(&x, 77), 10.0, 480.0, 1.3);
        assert_ne!(
            plain_lag(&reference, &take, 1000),
            77,
            "plain no longer fails"
        );
        let a = find(&reference, &take, 1000);
        assert_eq!(a.lag, 77);
        assert!(a.confidence.get() > 0.3, "{:?}", a.confidence);
    }

    /// The same on a source whose spectrum falls 12 dB per octave, with a hum
    /// well above it: whitening has to span the source's own dynamic range, and
    /// plain correlation has neither the hum nor the tilt in its favour.
    ///
    /// Mutation checked: no normalisation (as above) fails; so does a floor of
    /// 1e-2 (`PHAT_FLOOR`), which damps the weak high bins of the tilt.
    #[test]
    fn phat_keeps_the_delay_of_a_steeply_tilted_source_under_a_hum() {
        let x = tilted_noise(8192, 21);
        let reference = with_tone(x.clone(), 100.0, 480.0, 0.0);
        let take = with_tone(shifted(&x, 77), 100.0, 480.0, 1.3);
        assert_ne!(
            plain_lag(&reference, &take, 1000),
            77,
            "plain no longer fails"
        );
        let a = find(&reference, &take, 1000);
        assert_eq!(a.lag, 77);
        assert!(a.confidence.get() > 0.5, "{:?}", a.confidence);
    }

    /// A band keeps what two takes share: a narrow common signal under
    /// independent broadband noise is sharper within the band than outside it.
    ///
    /// Mutation checked: treating every bin as in band (`in_band` always
    /// true) makes the two readings equal and fails.
    #[test]
    fn a_band_limit_leaves_out_what_does_not_correlate() {
        let n = 2048;
        let mut rng = StdRng::seed_from_u64(31);
        let partials: Vec<(f32, f32)> = (0..12)
            .map(|_| {
                (
                    rng.gen_range(0.04..0.08),
                    rng.gen_range(0.0..core::f32::consts::TAU),
                )
            })
            .collect();
        let common = |t: f32| -> f32 {
            partials
                .iter()
                .map(|&(f, p)| (core::f32::consts::TAU * f * t + p).sin())
                .sum()
        };
        let (a, b) = (noise(n, 41), noise(n, 42));
        let reference: Vec<f32> = (0..n).map(|i| 3.0 * a[i] + common(i as f32)).collect();
        let take: Vec<f32> = (0..n)
            .map(|i| 3.0 * b[i] + common(i as f32 - 77.0))
            .collect();

        let mut fft = FftScratch::new();
        let open = AlignConfig::new(Samples(500));
        let banded = open
            .with_band(SampleRate::new(1.0), Hz::new(0.03), Hz::new(0.09))
            .unwrap();
        assert_eq!(banded.band(), Some((Hz(0.03), Hz(0.09))));
        let open = align(&reference, &take, &open, &mut fft).unwrap();
        let banded = align(&reference, &take, &banded, &mut fft).unwrap();
        assert_eq!(banded.lag, 77);
        assert!(
            banded.confidence.get() > 2.0 * open.confidence.get(),
            "banded {:?}, open {:?}",
            banded.confidence,
            open.confidence
        );
    }

    /// Out-of-band energy does not set the PHAT level: with bins a million
    /// times stronger outside the band, the in-band bins still come out at
    /// unit magnitude, and out-of-band bins are zero.
    ///
    /// Mutation checked: taking `strongest` over every bin (dropping the
    /// `in_band` filter) leaves the in-band bins near 1e-2 and fails.
    #[test]
    fn out_of_band_energy_does_not_set_the_level() {
        // 9 bins of a 16-point transform; at rate 16, bin `k` is `k` Hz.
        let mut bins = vec![Complex::new(3.0, -4.0); 9];
        bins[1] = Complex::new(5.0e6, 0.0);
        let band = AlignConfig::new(Samples(1))
            .with_band(SampleRate::new(16.0), Hz::new(4.0), Hz::new(6.0))
            .unwrap()
            .band;
        assert!(phase_transform(&mut bins, band));
        for (k, bin) in bins.iter().enumerate() {
            if (4..=6).contains(&k) {
                assert!((bin.norm() - 1.0).abs() < 1e-3, "bin {k}: {bin}");
                assert!((bin.re - 0.6).abs() < 1e-3 && (bin.im + 0.8).abs() < 1e-3);
            } else {
                assert_eq!(*bin, Complex::default(), "bin {k}");
            }
        }
    }

    /// A decaying, reverb-like filter on the take leaves the lag where it was.
    ///
    /// Mutation checked: negating the reported lag fails the pin. It is a
    /// robustness pin, not a discriminator: plain correlation also finds this
    /// lag, and the hum tests are the ones that tell PHAT from it.
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

    /// Only lags within the bound are chosen, the bound itself included; a
    /// window of one lag has no competitor and reads no confidence.
    ///
    /// Mutation checked: searching the whole overlap (`peak()` in place of
    /// `peak_within`) returns 200 and fails; an exclusive bound fails the
    /// edge case; returning `1.0` for an empty competitor set (`map_or(
    /// Confidence::CERTAIN, ..)`) fails the last assertion.
    #[test]
    fn the_max_lag_bound_is_respected() {
        let x = noise(2048, 6);
        let take = shifted(&x, 200);
        assert_eq!(find(&x, &take, 200).lag, 200);
        let a = find(&x, &take, 150);
        assert!(a.lag.abs() <= 150, "lag {}", a.lag);
        let one = find(&x, &x, 0);
        assert_eq!(one.lag, 0);
        assert_eq!(one.confidence, Confidence::NONE);
    }

    /// A peak that spans several lags (a band-limited signal, a perfect delay)
    /// still reads as clear: the flanks of its lobe are not a second peak.
    ///
    /// Mutation checked: skipping both walks along the lobe (`while at != limit`
    /// loops removed from `edge`) puts the flank in as the second peak and the
    /// confidence falls to about 0.06 and fails.
    #[test]
    fn a_wide_peak_is_not_its_own_runner_up() {
        let cfg = AlignConfig::new(Samples(300))
            .with_band(SampleRate::new(48_000.0), Hz::new(1000.0), Hz::new(4000.0))
            .unwrap();
        let x = band_noise(4096, 5, 1000.0 / 48_000.0, 4000.0 / 48_000.0);
        let mut fft = FftScratch::new();
        let clean = align(&x, &shifted(&x, 40), &cfg, &mut fft).unwrap();
        assert_eq!(clean.lag, 40);
        assert!(clean.confidence.get() > 0.7, "{:?}", clean.confidence);

        let (n1, n2) = (noise(4096, 6), noise(4096, 7));
        let reference: Vec<f32> = x.iter().zip(&n1).map(|(a, b)| a + 0.5 * b).collect();
        let take: Vec<f32> = shifted(&x, 40)
            .iter()
            .zip(&n2)
            .map(|(a, b)| a + 0.5 * b)
            .collect();
        let noisy = align(&reference, &take, &cfg, &mut fft).unwrap();
        assert_eq!(noisy.lag, 40);
        assert!(noisy.confidence.get() > 0.7, "{:?}", noisy.confidence);
    }

    /// The lobe rule on hand-made profiles (peak at index 5, lag = index):
    /// ripple above half height stays in the lobe, the flank beyond half height
    /// is walked down before the runner-up is read, an exact tie at the top
    /// does not read as zero, and a lobe that fills the window has no runner-up.
    ///
    /// Mutation checked: without the `LOBE_FRACTION` loop the ripple case reads
    /// 0.05; without the descent loop the flank case reads 0.55; a
    /// `LOBE_FRACTION` of 2.0 (strict descent only) reads 0.0 on the tie and
    /// 0.05 on the ripple.
    #[test]
    fn the_lobe_rule_on_made_up_profiles() {
        let read = |profile: &[f32], lag: isize| {
            let hi = profile.len() as isize - 1;
            distinctness(
                |l| {
                    profile
                        .get(l as usize)
                        .copied()
                        .unwrap_or(f32::NEG_INFINITY)
                },
                lag,
                profile[lag as usize],
                (0, hi),
            )
            .get()
        };
        let ripple = [
            0.0, 0.0, 0.1, 0.5, 0.9, 1.0, 0.9, 0.95, 0.7, 0.3, 0.1, 0.35, 0.1,
        ];
        assert!((read(&ripple, 5) - 0.65).abs() < 1e-6);
        let flank = [0.0, 0.0, 0.1, 0.4, 0.8, 1.0, 0.8, 0.45, 0.3, 0.1, 0.25, 0.1];
        assert!((read(&flank, 5) - 0.75).abs() < 1e-6);
        let tie = [0.0, 0.1, 0.6, 1.0, 1.0, 0.6, 0.1, 0.0, 0.2, 0.0];
        assert!((read(&tie, 3) - 0.8).abs() < 1e-6);
        assert_eq!(read(&[1.0, 0.9, 0.8], 0), 0.0);
    }

    /// Unrelated noise has no distinct peak. Over the roughly 2000 lags of the
    /// window the top two of that many Gaussian-like values differ by about
    /// `1 / sqrt(2 ln n)` of a deviation against a maximum near `3.5`
    /// deviations, so confidence sits near 0.07; the bound allows four times
    /// that.
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

    /// The parabola is used only where the three points bend down: a delta one
    /// lag to the right of the fitted lag has `curvature > 0`, and NaN has
    /// neither sign; both give zero rather than a clamped nonsense offset.
    ///
    /// Mutation checked: `curvature != 0.0` in place of `curvature < 0.0`
    /// returns -0.5 for the delta (and NaN for the NaN) and fails.
    #[test]
    fn the_parabola_is_used_only_where_it_bends_down() {
        let mut fft = FftScratch::new();
        let x = noise(8, 1);
        let delta_at_one = cross_correlate_shaped(&x, &x, &mut fft, |bins| {
            let size = ((bins.len() - 1) * 2) as f32;
            for (k, bin) in bins.iter_mut().enumerate() {
                let w = -core::f32::consts::TAU * k as f32 / size;
                *bin = Complex::new(w.cos(), w.sin());
            }
        });
        assert_eq!(delta_at_one.peak().unwrap().lag, 1);
        assert_eq!(parabolic_offset(&delta_at_one, 0), 0.0);

        let nan = cross_correlate_shaped(&x, &x, &mut fft, |bins| {
            bins.fill(Complex::new(f32::NAN, 0.0));
        });
        assert_eq!(parabolic_offset(&nan, 0), 0.0);
    }

    /// Delay in samples and seconds, and the input checks.
    ///
    /// Mutation checked: scaling the seconds by two fails the figure;
    /// replacing the empty-input, finite, Nyquist, range, rate or silence
    /// check with `if false` fails the matching assertion.
    #[test]
    fn delay_converts_and_bad_input_is_refused() {
        let a = Alignment {
            lag: 48,
            offset: 0.25,
            confidence: Confidence::CERTAIN,
        };
        assert_eq!(a.delay_samples(), 48.25);
        assert!((a.delay(SampleRate::new(48_000.0)).get() - 48.25 / 48_000.0).abs() < 1e-9);

        let mut fft = FftScratch::new();
        let cfg = AlignConfig::new(Samples(10));
        assert_eq!(cfg.band(), None);
        let one = [1.0f32];
        assert!(matches!(
            align(&[], &one, &cfg, &mut fft),
            Err(Error::InsufficientInput { .. })
        ));
        assert!(matches!(
            align(&one, &[], &cfg, &mut fft),
            Err(Error::InsufficientInput { .. })
        ));
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let x = [1.0, bad, 2.0];
            assert_eq!(align(&x, &one, &cfg, &mut fft), Err(Error::NonFiniteInput));
            assert_eq!(align(&one, &x, &cfg, &mut fft), Err(Error::NonFiniteInput));
        }
        // Silence, and a band between two bins: nothing to correlate.
        let quiet = [0.0f32; 64];
        let x = noise(64, 3);
        assert_eq!(align(&x, &quiet, &cfg, &mut fft), Err(Error::NoSignal));
        assert_eq!(align(&quiet, &x, &cfg, &mut fft), Err(Error::NoSignal));
        let gap = cfg
            .with_band(SampleRate::new(48_000.0), Hz::new(100.0), Hz::new(200.0))
            .unwrap();
        assert_eq!(align(&x, &x, &gap, &mut fft), Err(Error::NoSignal));

        assert_eq!(
            cfg.with_band(SampleRate::new(48_000.0), Hz::new(100.0), Hz::new(30_000.0)),
            Err(Error::AboveNyquist {
                freq: Hz(30_000.0),
                nyquist: Hz(24_000.0)
            })
        );
        assert!(matches!(
            cfg.with_band(SampleRate::new(48_000.0), Hz::new(500.0), Hz::new(100.0)),
            Err(Error::EmptyFrequencyRange { .. })
        ));
        assert_eq!(
            cfg.with_band(SampleRate::new(0.0), Hz::new(100.0), Hz::new(200.0)),
            Err(Error::NonPositiveSampleRate)
        );
    }
}
