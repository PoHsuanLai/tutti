use super::*;
use crate::testing::{amplitude_at, lock_in};
use tutti_fixtures::signal::sine;
use tutti_types::{Amplitude, Hz, SampleRate, Samples};

const RATE: f64 = 48_000.0;
/// A whole number of periods of every tone on the 5 Hz grid.
const WINDOW: usize = 9_600;
const SETTLE: usize = 12_000;
/// f32 rounding of the input, each band, and the sum: at most
/// `(bands + 2)` half-ulps of the peak, each `2^-24`, which the lock-in
/// only averages down.
const TOL: f64 = 1e-6;

/// A unit sine at `hz` (on the 5 Hz grid, exact in `f32`) at `rate`.
fn unit_sine(hz: f64, rate: f64, len: usize) -> Vec<f32> {
    sine(
        Hz::new(hz as f32),
        Amplitude::new(1.0),
        SampleRate::of(rate),
        Samples(len),
    )
}

/// The 5 Hz-grid frequencies of a log sweep from 20 Hz to 16 kHz.
fn sweep() -> Vec<f64> {
    (0..48)
        .map(|i| {
            let f = 20.0 * (16_000.0f64 / 20.0).powf(i as f64 / 47.0);
            (f / 5.0).round() * 5.0
        })
        .collect()
}

/// Band `b`'s phasor and the bands' summed amplitude for a unit tone at
/// `hz`, in steady state: the tree of `freqs` at 48 kHz run as the crossover runs it.
fn bands_at(freqs: &[f32], hz: f64) -> (Vec<(f64, f64)>, f64) {
    let freqs: Vec<Hz> = freqs.iter().map(|&f| Hz(f)).collect();
    let model = CrossoverModel::new(&freqs, SampleRate::of(RATE)).unwrap();
    let mut state = CrossoverState::default();
    let x = unit_sine(hz, RATE, SETTLE + WINDOW);
    let nb = model.bands();
    let mut bands = vec![Vec::with_capacity(WINDOW); nb];
    let mut sum = Vec::with_capacity(WINDOW);
    for (i, &s) in x.iter().enumerate() {
        let out = state.tick(&model, s);
        if i >= SETTLE {
            for (b, v) in bands.iter_mut().zip(out) {
                b.push(v);
            }
            sum.push(out[..nb].iter().sum::<f32>());
        }
    }
    (
        bands.iter().map(|b| lock_in(b, hz, RATE)).collect(),
        amplitude_at(&sum, hz, RATE),
    )
}

fn mag(z: (f64, f64)) -> f64 {
    z.0.hypot(z.1)
}

fn assert_flat(freqs: &[f32]) {
    for hz in sweep() {
        let (_, sum) = bands_at(freqs, hz);
        assert!(
            (sum - 1.0).abs() < TOL,
            "{} bands at {hz} Hz: the sum has amplitude {sum}",
            freqs.len() + 1
        );
    }
}

/// The two branches sum to unity magnitude across the audio band.
#[test]
fn two_bands_sum_to_flat_magnitude() {
    assert_flat(&[1_000.0]);
}

/// With the all-pass compensation, three and four bands sum flat too.
#[test]
fn three_and_four_bands_sum_to_flat_magnitude() {
    assert_flat(&[120.0, 2_500.0]);
    assert_flat(&[120.0, 1_000.0, 6_000.0]);
}

/// Six bands, the most, sum flat.
#[test]
fn six_bands_sum_to_flat_magnitude() {
    assert_flat(&[100.0, 300.0, 1_000.0, 3_000.0, 9_000.0]);
}

/// Each branch is 6.02 dB down at the crossover (half amplitude) and the
/// two are in phase there.
#[test]
fn each_branch_is_6_02_db_down_and_in_phase_at_the_crossover() {
    let (bands, _) = bands_at(&[1_000.0], 1_000.0);
    let (low, high) = (bands[0], bands[1]);
    assert!((mag(low) - 0.5).abs() < TOL, "low {}", mag(low));
    assert!((mag(high) - 0.5).abs() < TOL, "high {}", mag(high));
    // z_low / z_high = 1 when equal in magnitude and phase.
    let d = low.0 * high.0 + low.1 * high.1;
    let c = low.1 * high.0 - low.0 * high.1;
    let norm = high.0 * high.0 + high.1 * high.1;
    assert!((d / norm - 1.0).abs() < 4.0 * TOL, "ratio re {}", d / norm);
    assert!((c / norm).abs() < 4.0 * TOL, "ratio im {}", c / norm);
}

/// Each branch's magnitude is the analytic 4th-order one: low
/// `1 / (1 + x^4)`, high `x^4 / (1 + x^4)`, with `x` the frequency ratio
/// as the bilinear transform warps it, `tan(pi f / fs) / tan(pi fc / fs)`.
#[test]
fn branch_magnitudes_are_the_butterworth_squared_curve() {
    for hz in [100.0, 500.0, 800.0, 1_000.0, 1_250.0, 2_000.0, 8_000.0] {
        let (bands, _) = bands_at(&[1_000.0], hz);
        let warp = |f: f64| libm::tan(core::f64::consts::PI * f / RATE);
        let x4 = (warp(hz) / warp(1_000.0)).powi(4);
        assert!(
            (mag(bands[0]) - 1.0 / (1.0 + x4)).abs() < TOL,
            "low at {hz}"
        );
        assert!(
            (mag(bands[1]) - x4 / (1.0 + x4)).abs() < TOL,
            "high at {hz}"
        );
    }
}

/// Every band of a 3- or 4-band split has the same phase at every
/// frequency it carries: `LP^2`, `HP^2` and the all-pass all have phase
/// `-2 arg D`. Compared where a band has at least 0.1 of the tone (a
/// weaker band's phase is below the lock-in's resolution of `TOL / 0.1`).
#[test]
fn every_band_has_the_same_phase() {
    for freqs in [vec![120.0, 2_500.0], vec![120.0, 1_000.0, 6_000.0]] {
        let mut compared = 0;
        for hz in [
            100.0, 120.0, 200.0, 500.0, 1_000.0, 2_500.0, 4_000.0, 6_000.0, 9_000.0,
        ] {
            let (bands, _) = bands_at(&freqs, hz);
            let loud: Vec<_> = bands.iter().filter(|z| mag(**z) > 0.1).collect();
            for pair in loud.windows(2) {
                let (a, b) = (pair[0], pair[1]);
                let cross = (a.0 * b.1 - a.1 * b.0) / (mag(*a) * mag(*b));
                let dot = (a.0 * b.0 + a.1 * b.1) / (mag(*a) * mag(*b));
                assert!(
                    cross.abs() < 2e-5 && dot > 0.0,
                    "{freqs:?} at {hz}: {cross} {dot}"
                );
                compared += 1;
            }
        }
        assert!(
            compared >= 3,
            "{freqs:?}: only {compared} overlapping pairs"
        );
    }
}
