//! FFT cross-correlation of two buffers.
//!
//! The same Wiener-Khinchin route the pitch detector takes for
//! autocorrelation, for two signals: zero-pad both to a power of two that holds
//! the whole linear result, multiply one spectrum by the conjugate of the
//! other, transform back. Cost is `O(n log n)` rather than the `O(n * lags)` of
//! the direct sum. A caller that wants a weighted variant (phase transform)
//! reshapes the cross-spectrum through [`cross_correlate_shaped`].

use tutti_types::Samples;

use tutti_dsp::Complex;
use tutti_dsp::FftScratch;

/// The linear cross-correlation of two buffers, at every lag they overlap.
///
/// `at(lag)` is `sum_n a[n + lag] * b[n]`, so a **positive lag means `a` is a
/// delayed copy of `b`**. It is not normalised: the zero lag of a buffer with
/// itself is its energy.
///
/// Lags are `isize`: [`Samples`] is unsigned, and a correlation is defined
/// either side of zero.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CrossCorrelation {
    values: Vec<f32>,
    /// Index of lag zero in `values`: `b.len() - 1`.
    zero: usize,
}

/// The strongest lag of a [`CrossCorrelation`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CorrelationPeak {
    /// Where the maximum sits, in whole samples; see [`CrossCorrelation`] for
    /// the sign.
    pub lag: isize,
    /// The correlation there, in the units of the product of the two inputs.
    pub value: f32,
}

impl CrossCorrelation {
    /// The values from the most negative lag to the most positive, one per
    /// lag: `a.len() + b.len() - 1` of them, or none if either input was
    /// empty.
    #[inline]
    pub fn values(&self) -> &[f32] {
        &self.values
    }

    /// The most negative lag held (`-(b.len() - 1)`), or zero when empty.
    #[inline]
    pub fn min_lag(&self) -> isize {
        -(self.zero as isize)
    }

    /// The most positive lag held (`a.len() - 1`), or zero when empty.
    #[inline]
    pub fn max_lag(&self) -> isize {
        self.values.len().saturating_sub(1) as isize + self.min_lag()
    }

    /// The correlation at `lag`, or `None` outside the overlap.
    #[inline]
    pub fn at(&self, lag: isize) -> Option<f32> {
        let index = lag.checked_add(self.zero as isize)?;
        usize::try_from(index)
            .ok()
            .and_then(|i| self.values.get(i))
            .copied()
    }

    /// The largest value over every lag.
    #[inline]
    pub fn peak(&self) -> Option<CorrelationPeak> {
        self.peak_within(Samples(usize::MAX))
    }

    /// The largest value over lags `-max_lag..=max_lag`, the first on a tie.
    ///
    /// `None` when that window holds no lag. A bound beyond the overlap is
    /// clamped to it.
    pub fn peak_within(&self, max_lag: Samples) -> Option<CorrelationPeak> {
        let bound = isize::try_from(max_lag.get()).unwrap_or(isize::MAX);
        let (lo, hi) = (self.min_lag().max(-bound), self.max_lag().min(bound));
        (lo..=hi)
            .filter_map(|lag| self.at(lag).map(|value| CorrelationPeak { lag, value }))
            .fold(None, |best: Option<CorrelationPeak>, next| match best {
                Some(b) if b.value >= next.value => Some(b),
                _ => Some(next),
            })
    }
}

/// Cross-correlates `a` against `b` through the FFT.
///
/// See [`CrossCorrelation`] for the lag convention. Allocates the padded
/// buffers and the result; an offline call.
pub fn cross_correlate(a: &[f32], b: &[f32], fft: &mut FftScratch) -> CrossCorrelation {
    cross_correlate_shaped(a, b, fft, |_| {})
}

/// [`cross_correlate`] with the cross-spectrum `A * conj(B)` handed to `shape`
/// before the inverse transform, for a weighting such as the phase transform.
///
/// `shape` sees `n / 2 + 1` bins, `n` the power of two that holds the whole
/// linear result. A phase transform that divides by the bin magnitude should
/// add an epsilon relative to the spectrum's own level, not an absolute one,
/// so quiet and loud inputs weight alike.
pub(crate) fn cross_correlate_shaped(
    a: &[f32],
    b: &[f32],
    fft: &mut FftScratch,
    shape: impl FnOnce(&mut [Complex]),
) -> CrossCorrelation {
    if a.is_empty() || b.is_empty() {
        return CrossCorrelation::default();
    }
    let len = a.len() + b.len() - 1;
    // At least 2: a one-point real transform has no distinct Nyquist bin.
    let size = len.next_power_of_two().max(2);
    let bins = size / 2 + 1;
    let ones = vec![1.0f32; size];

    let spectrum = |signal: &[f32], fft: &mut FftScratch| {
        let mut padded = vec![0.0f32; size];
        padded[..signal.len()].copy_from_slice(signal);
        let mut out = vec![Complex::default(); bins];
        fft.forward(&padded, &ones, &mut out);
        out
    };
    let mut cross = spectrum(a, fft);
    let other = spectrum(b, fft);
    for (x, y) in cross.iter_mut().zip(&other) {
        *x *= y.conj();
    }
    shape(&mut cross);

    let mut circular = vec![0.0f32; size];
    fft.inverse(&cross, &mut circular);

    // Negative lags wrapped to the end of the circular result; lay them out in
    // front of the non-negative ones.
    let zero = b.len() - 1;
    let values = (0..len)
        .map(|i| circular[(i + size - zero) % size])
        .collect();
    CrossCorrelation { values, zero }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, SeedableRng, rngs::StdRng};

    fn noise(len: usize, seed: u64) -> Vec<f32> {
        let mut rng = StdRng::seed_from_u64(seed);
        (0..len).map(|_| rng.gen_range(-1.0..1.0)).collect()
    }

    /// A hand-worked case: `a = [1, 2, 3]`, `b = [0, 1, 0.5]`, with
    /// `r[lag] = sum a[n + lag] b[n]` summed out lag by lag below.
    ///
    /// Mutation checked: correlating `b` against `a` (swapping the operands, so
    /// the conjugate lands on the other spectrum) fails every non-symmetric
    /// lag.
    #[test]
    fn a_small_case_matches_the_hand_worked_sums() {
        let mut fft = FftScratch::new();
        let r = cross_correlate(&[1.0, 2.0, 3.0], &[0.0, 1.0, 0.5], &mut fft);
        // lag -2: a[0]b[2]                 = 0.5
        // lag -1: a[0]b[1] + a[1]b[2]      = 1 + 1   = 2
        // lag  0: a[0]b[0] + a[1]b[1] + a[2]b[2] = 0 + 2 + 1.5 = 3.5
        // lag  1: a[1]b[0] + a[2]b[1]      = 3
        // lag  2: a[2]b[0]                 = 0
        let expected = [0.5, 2.0, 3.5, 3.0, 0.0];
        assert_eq!((r.min_lag(), r.max_lag()), (-2, 2));
        for (lag, want) in (-2..=2).zip(expected) {
            let got = r.at(lag).unwrap();
            assert!((got - want).abs() < 1e-5, "lag {lag}: {got} vs {want}");
        }
        assert_eq!(r.at(3), None);
        assert_eq!(r.at(-3), None);
    }

    /// A delayed copy peaks at exactly its delay, positive when `a` lags `b`
    /// and negative the other way, and unequal lengths work.
    ///
    /// Mutation checked: flipping the sign of the lag mapping (`i - zero` to
    /// `zero - i`) fails both directions; halving the padded size
    /// (`next_power_of_two() / 2`) makes the extreme lags wrap and fails.
    #[test]
    fn a_delayed_copy_peaks_at_its_delay_in_either_direction() {
        let mut fft = FftScratch::new();
        let base = noise(700, 7);
        let delay = 123usize;

        let mut late = vec![0.0f32; delay];
        late.extend_from_slice(&base);
        let r = cross_correlate(&late, &base, &mut fft);
        assert_eq!(r.peak().unwrap().lag, 123);

        let r = cross_correlate(&base, &late, &mut fft);
        assert_eq!(r.peak().unwrap().lag, -123);

        // The longest lag in range still lands where it should.
        let tail: Vec<f32> = base[600..].to_vec();
        let r = cross_correlate(&base, &tail, &mut fft);
        assert_eq!(r.peak().unwrap().lag, 600);
    }

    /// A buffer against itself peaks at lag zero with its energy, and the
    /// result is symmetric about it.
    ///
    /// Mutation checked: scaling `circular` by 2 after the inverse fails the
    /// energy; conjugating neither spectrum (`*x *= *y`) fails the lag-zero
    /// peak.
    #[test]
    fn autocorrelation_peaks_at_zero_with_the_energy_and_is_symmetric() {
        let mut fft = FftScratch::new();
        let x = noise(513, 3);
        let r = cross_correlate(&x, &x, &mut fft);
        let energy: f32 = x.iter().map(|v| v * v).sum();
        let peak = r.peak().unwrap();
        assert_eq!(peak.lag, 0);
        assert!((peak.value - energy).abs() < energy * 1e-4);
        for lag in 1..=512 {
            let (p, n) = (r.at(lag).unwrap(), r.at(-lag).unwrap());
            assert!((p - n).abs() < energy * 1e-4, "lag {lag}: {p} vs {n}");
        }
    }

    /// `peak_within` ignores a stronger peak beyond the bound, clamps an
    /// oversized bound, and returns `None` for empty input.
    ///
    /// Mutation checked: ignoring the bound (always searching every lag) fails
    /// the first assertion; using `lo..hi` (exclusive) fails the edge lag.
    #[test]
    fn peak_within_respects_the_bound() {
        let mut fft = FftScratch::new();
        let base = noise(300, 11);
        let mut late = vec![0.0f32; 100];
        late.extend_from_slice(&base);
        let r = cross_correlate(&late, &base, &mut fft);
        assert_eq!(r.peak_within(Samples(100)).unwrap().lag, 100);
        assert_ne!(r.peak_within(Samples(99)).unwrap().lag, 100);
        assert_eq!(r.peak_within(Samples(usize::MAX)), r.peak());
        assert_eq!(cross_correlate(&[], &base, &mut fft).peak(), None);
        assert_eq!(cross_correlate(&base, &[], &mut fft).values().len(), 0);
    }

    /// The shaping hook sees the cross-spectrum before the inverse: zeroing it
    /// zeroes the result.
    ///
    /// Mutation checked: calling `shape` after the inverse transform (or not at
    /// all) leaves the values non-zero and fails.
    #[test]
    fn the_shape_hook_reaches_the_cross_spectrum() {
        let mut fft = FftScratch::new();
        let x = noise(64, 5);
        let r = cross_correlate_shaped(&x, &x, &mut fft, |bins| bins.fill(Complex::default()));
        assert!(r.values().iter().all(|&v| v == 0.0));
    }
}
