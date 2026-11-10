//! Spectral shape features: centroid, flatness and rolloff.
//!
//! Each is a pure function of one magnitude spectrum (`DC..=Nyquist`, one
//! value per bin), so it runs over a row of an [`StftMagnitude`] or any
//! magnitudes a caller already holds. A silent spectrum has no shape: every
//! feature reads zero rather than a division by nothing.

use tutti_types::Hz;

use crate::transform::StftMagnitude;
use tutti_dsp::{BinIndex, FrameIndex};

/// Power below this share of the frame's mean power is treated as this much
/// when taking a logarithm (-120 dB), so empty bins do not send the geometric
/// mean to zero by an amount that depends on the bin count. Relative, so a quiet
/// frame reads the same as a loud one.
const RELATIVE_POWER_FLOOR: f64 = 1e-12;

/// The magnitude-weighted mean frequency: where the spectrum's centre of mass
/// sits, and the usual proxy for brightness.
///
/// `bin_width` is the spacing of the bins in [`Hz`]
/// (`sample_rate / window`). `Hz(0.0)` for a silent spectrum, and for one with
/// a NaN or infinite magnitude.
pub fn spectral_centroid(magnitudes: &[f32], bin_width: Hz) -> Hz {
    let (weighted, total) = magnitudes
        .iter()
        .enumerate()
        .fold((0.0f64, 0.0f64), |(w, t), (k, &m)| {
            (w + k as f64 * f64::from(m), t + f64::from(m))
        });
    if total.is_finite() && total > 0.0 {
        Hz((weighted / total * f64::from(bin_width.get())) as f32)
    } else {
        Hz(0.0)
    }
}

/// Wiener entropy: the geometric over the arithmetic mean of the power
/// spectrum, `1` for a flat (noise-like) spectrum and toward `0` for a
/// tonal one. Bins under `1e-12` of the mean power count as that much, so the
/// result depends on the spectrum's shape and not on its level.
///
/// A plain `f32` in `0..=1`, not a unit type: no unit covers a 0..1 spectral
/// ratio. `Confidence` is an estimator's certainty and `Mix` is a blend
/// control, and a new unit needs a distinct range or algebra.
///
/// `0.0` for a silent spectrum, and for one with a NaN or infinite magnitude.
pub fn spectral_flatness(magnitudes: &[f32]) -> f32 {
    if magnitudes.is_empty() {
        return 0.0;
    }
    let power = |m: f32| f64::from(m) * f64::from(m);
    let mean: f64 = magnitudes.iter().map(|&m| power(m)).sum::<f64>() / magnitudes.len() as f64;
    if !mean.is_finite() || mean <= 0.0 {
        return 0.0;
    }
    let floor = mean * RELATIVE_POWER_FLOOR;
    let mean_log: f64 = magnitudes
        .iter()
        .map(|&m| power(m).max(floor).ln())
        .sum::<f64>()
        / magnitudes.len() as f64;
    // Both means in the log domain: exp(mean_log) / mean.
    ((mean_log - mean.ln()).exp() as f32).clamp(0.0, 1.0)
}

/// The frequency below which `fraction` of the spectrum's energy lies: the
/// centre of the first bin at which the running sum of squared magnitudes
/// reaches `fraction` of the total.
///
/// `fraction` is a share of a whole in `0..=1` (85 % is the usual choice,
/// `0.85`); a value outside it acts as the nearer end and a NaN as `0.0`. A plain
/// `f32`, not a unit type, for the reason given at [`spectral_flatness`]: no
/// unit covers a 0..1 spectral ratio, and `Mix` is a blend control.
///
/// `Hz(0.0)` for a silent spectrum, and for one with a NaN or infinite
/// magnitude. At `1.0` this is the highest bin that holds any energy.
pub fn spectral_rolloff(magnitudes: &[f32], bin_width: Hz, fraction: f32) -> Hz {
    let energy = |m: f32| f64::from(m) * f64::from(m);
    let total: f64 = magnitudes.iter().map(|&m| energy(m)).sum();
    if !total.is_finite() || total <= 0.0 {
        return Hz(0.0);
    }
    let fraction = if fraction.is_nan() { 0.0 } else { fraction };
    let target = total * f64::from(fraction);
    let mut running = 0.0f64;
    let mut last_active = 0usize;
    for (k, &m) in magnitudes.iter().enumerate() {
        if m > 0.0 {
            last_active = k;
        }
        running += energy(m);
        if running >= target {
            break;
        }
    }
    // The bin that crossed the target, or, if the float sum fell a hair short
    // of the total at a full share, the last one that holds energy.
    Hz(last_active as f32 * bin_width.get())
}

/// The three features of one frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpectralFeatures {
    /// See [`spectral_centroid`].
    pub centroid: Hz,
    /// See [`spectral_flatness`].
    pub flatness: f32,
    /// See [`spectral_rolloff`].
    pub rolloff: Hz,
}

/// The features of every frame of a magnitude STFT, in frame order.
///
/// `rolloff` is the energy share passed to [`spectral_rolloff`], as described there. The bin width
/// comes from the transform's own geometry, so it cannot disagree with the grid.
pub fn spectral_features(transform: &StftMagnitude, rolloff: f32) -> Vec<SpectralFeatures> {
    let geometry = transform.geometry();
    let bin_width = geometry.bin_frequency(BinIndex(1));
    let grid = transform.magnitudes().grid();
    grid.frames()
        .indices()
        .map(|frame: FrameIndex| {
            let row = grid.row(frame);
            SpectralFeatures {
                centroid: spectral_centroid(row, bin_width),
                flatness: spectral_flatness(row),
                rolloff: spectral_rolloff(row, bin_width, rolloff),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{StftRequest, stft_magnitude};
    use rand::{Rng, SeedableRng, rngs::StdRng};
    use tutti_dsp::FftScratch;
    use tutti_types::SampleRate;
    use tutti_types::Samples;

    const RATE: f64 = 48_000.0;
    const WINDOW: usize = 2048;

    fn magnitudes_of(samples: &[f32]) -> StftMagnitude {
        let request = StftRequest::new(SampleRate::new(RATE), Samples(WINDOW), Samples(WINDOW / 2));
        stft_magnitude(samples, request, &mut FftScratch::new()).unwrap()
    }

    fn bin_width() -> Hz {
        Hz((RATE / WINDOW as f64) as f32)
    }

    fn sine(hz: f32, len: usize) -> Vec<f32> {
        (0..len)
            .map(|i| (core::f32::consts::TAU * hz * i as f32 / RATE as f32).sin())
            .collect()
    }

    /// A pure sine's centroid is its frequency, to within a bin.
    ///
    /// Mutation checked: weighting by `k + 1` puts the centroid one bin high
    /// and fails the one-bin tolerance.
    #[test]
    fn a_sine_centroid_is_its_frequency_within_a_bin() {
        for hz in [1000.0f32, 3517.0, 9000.0] {
            let stft = magnitudes_of(&sine(hz, 16_384));
            let features = spectral_features(&stft, 0.85);
            assert!(!features.is_empty());
            for f in &features {
                assert!(
                    (f.centroid.get() - hz).abs() < bin_width().get(),
                    "{hz}: {}",
                    f.centroid
                );
            }
        }
    }

    /// Two equal bins at 10 and 30 put the centroid midway, exactly.
    ///
    /// Mutation checked: dividing by the bin count instead of the magnitude
    /// total fails.
    #[test]
    fn the_centroid_of_two_equal_bins_is_their_midpoint() {
        let mut row = vec![0.0f32; 65];
        row[10] = 2.0;
        row[30] = 2.0;
        let c = spectral_centroid(&row, Hz(10.0));
        assert!((c.get() - 200.0).abs() < 1e-3, "{c}");
    }

    /// White noise is flat, a sine is not, and a perfectly flat row is 1.
    ///
    /// Mutation checked: taking the square root of the ratio (a magnitude
    /// rather than a power flatness) moves noise to about 0.75 and fails the
    /// bracket.
    #[test]
    fn noise_is_flat_a_sine_is_not() {
        assert_eq!(spectral_flatness(&[3.0; 100]), 1.0);

        let mut rng = StdRng::seed_from_u64(42);
        let noise: Vec<f32> = (0..65_536).map(|_| rng.gen_range(-1.0..1.0)).collect();
        let flat = spectral_features(&magnitudes_of(&noise), 0.85);
        let mean: f32 = flat.iter().map(|f| f.flatness).sum::<f32>() / flat.len() as f32;
        // The geometric mean of an exponential power distribution sits at
        // exp(-gamma) = 0.56 of its mean.
        assert!((0.5..0.62).contains(&mean), "noise flatness {mean}");

        let tone = spectral_features(&magnitudes_of(&sine(1000.0, 16_384)), 0.85);
        assert!(tone.iter().all(|f| f.flatness < 1e-3), "{tone:?}");
    }

    /// A row flat over bins 0..100 rolls off at the energy share's bin, and at
    /// a full share at the band edge.
    ///
    /// 100 equal bins: half the energy is reached at bin 49 (exactly 50 of 100,
    /// so the comparison is `>=`), 85 % needs 86 bins, so bin 85.
    ///
    /// Mutation checked: `>` for `>=` at the target gives bin 50 and fails;
    /// reporting the bin's upper edge (`k + 1`) fails; returning the length of
    /// the row instead of the last active bin fails the full share.
    #[test]
    fn rolloff_sits_at_the_energy_share_and_the_band_edge() {
        let mut row = vec![0.0f32; 513];
        row[..100].fill(1.0);
        let width = Hz(10.0);
        assert_eq!(spectral_rolloff(&row, width, 0.5), Hz(490.0));
        assert_eq!(spectral_rolloff(&row, width, 0.85), Hz(850.0));
        assert_eq!(spectral_rolloff(&row, width, 1.0), Hz(990.0));
        assert_eq!(spectral_rolloff(&row, width, 0.0), Hz(0.0));
    }

    /// Band-limited noise (unit magnitude below an edge, random phase) rolls
    /// off at its band edge at 99 %.
    ///
    /// Mutation checked: replacing `fraction.get()` by `0.5` in the target
    /// puts the rolloff at half the band and fails the lower bound.
    #[test]
    fn band_limited_noise_rolls_off_at_the_edge() {
        let mut rng = StdRng::seed_from_u64(9);
        let mut fft = FftScratch::new();
        let size = 16_384;
        let edge_bin = 1000usize; // 5859 Hz at 48 kHz / 16384
        let mut bins = vec![tutti_dsp::Complex::default(); size / 2 + 1];
        for b in bins.iter_mut().take(edge_bin).skip(1) {
            *b = tutti_dsp::Complex::from_polar(1.0, rng.gen_range(0.0..core::f32::consts::TAU));
        }
        let mut x = vec![0.0f32; size];
        fft.inverse(&bins, &mut x);
        let width = RATE as f32 / size as f32;
        let row = {
            let mut out = vec![tutti_dsp::Complex::default(); size / 2 + 1];
            fft.forward(&x, &vec![1.0; size], &mut out);
            out.iter().map(|b| b.norm()).collect::<Vec<_>>()
        };
        let r = spectral_rolloff(&row, Hz(width), 0.99);
        let edge = edge_bin as f32 * width;
        assert!(r.get() <= edge && r.get() > 0.98 * edge, "{r} vs {edge}");
    }

    /// Flatness depends on the spectrum's shape, not its level, and zero bins in
    /// a tonal frame do not make it depend on the bin count.
    ///
    /// Mutation checked: an absolute floor (`let floor = 1e-20`) fails the
    /// scaled case (the 1e-12 row falls under it and reads 1); a floor of
    /// `mean * 1e-2` fails the tonal bound.
    #[test]
    fn flatness_is_level_independent_and_a_zero_bin_tonal_frame_is_near_zero() {
        let mut rng = StdRng::seed_from_u64(1);
        let row: Vec<f32> = (0..1025).map(|_| rng.gen_range(0.2..1.0)).collect();
        let quiet: Vec<f32> = row.iter().map(|m| m * 1e-12).collect();
        let (loud, soft) = (spectral_flatness(&row), spectral_flatness(&quiet));
        assert!(loud > 0.5 && (loud - soft).abs() < 1e-4, "{loud} vs {soft}");

        for bins in [65usize, 1025, 16_385] {
            let mut tonal = vec![0.0f32; bins];
            tonal[bins / 3] = 1.0;
            let f = spectral_flatness(&tonal);
            assert!(f < 1e-9, "{bins} bins: {f}");
        }
    }

    /// A NaN or infinite magnitude gives no result rather than a NaN, and a
    /// rolloff share outside `0..=1` acts as its nearer end.
    ///
    /// Mutation checked: removing the `is_finite` guards fails the NaN and
    /// infinity rows (NaN centroid, a rolloff scan to the last bin); removing
    /// the NaN case fails the last line. (A clamp is not needed: a share above 1
    /// is never reached and one below 0 is met at once.)
    #[test]
    fn non_finite_magnitudes_give_no_result_and_an_out_of_range_share_acts_as_its_nearer_end() {
        for bad in [f32::NAN, f32::INFINITY] {
            let mut row = vec![1.0f32; 32];
            row[5] = bad;
            assert_eq!(spectral_centroid(&row, Hz(10.0)), Hz(0.0));
            assert_eq!(spectral_flatness(&row), 0.0);
            assert_eq!(spectral_rolloff(&row, Hz(10.0), 0.85), Hz(0.0));
        }
        let mut row = vec![0.0f32; 40];
        row[..20].fill(1.0);
        assert_eq!(spectral_rolloff(&row, Hz(10.0), 7.0), Hz(190.0));
        assert_eq!(spectral_rolloff(&row, Hz(10.0), -3.0), Hz(0.0));
        assert_eq!(spectral_rolloff(&row, Hz(10.0), f32::NAN), Hz(0.0));
    }

    /// A silent spectrum reads zero on all three, and an empty one does not
    /// panic.
    ///
    /// Mutation checked: removing the `total > 0.0` guard in the centroid gives
    /// `NaN` and fails.
    #[test]
    fn silence_has_no_shape() {
        let zeros = [0.0f32; 33];
        assert_eq!(spectral_centroid(&zeros, Hz(5.0)), Hz(0.0));
        assert_eq!(spectral_flatness(&zeros), 0.0);
        assert_eq!(spectral_rolloff(&zeros, Hz(5.0), 0.85), Hz(0.0));
        assert_eq!(spectral_flatness(&[]), 0.0);
        assert_eq!(spectral_centroid(&[], Hz(5.0)), Hz(0.0));
    }

    /// The grid helper takes its bin width from the transform: its output is
    /// the per-row kernels' on the transform's own rows.
    ///
    /// Mutation checked: using `sample_rate / hop` for the width scales the
    /// centroid by 2 and fails.
    #[test]
    fn the_grid_helper_applies_the_kernels_row_by_row() {
        let stft = magnitudes_of(&sine(2000.0, 8192));
        let width = stft.geometry().bin_frequency(BinIndex(1));
        let grid = stft.magnitudes().grid();
        let features = spectral_features(&stft, 0.5);
        assert_eq!(features.len(), grid.frames().get());
        for (frame, f) in grid.frames().indices().zip(&features) {
            let row = grid.row(frame);
            assert_eq!(f.centroid, spectral_centroid(row, width));
            assert_eq!(f.flatness, spectral_flatness(row));
            assert_eq!(f.rolloff, spectral_rolloff(row, width, 0.5));
        }
        assert!((width.get() - 23.4375).abs() < 1e-4);
    }
}
