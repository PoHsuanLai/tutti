use super::*;
use crate::testing::{amplitude_at, lock_in, sine};

const RATE: f64 = 48_000.0;
/// A whole number of periods of every tone on the 5 Hz grid.
const WINDOW: usize = 9_600;
const SETTLE: usize = 12_000;
/// f32 rounding of the input, each band, and the sum: at most
/// `(bands + 2)` half-ulps of the peak, each `2^-24`, which the lock-in
/// only averages down.
const TOL: f64 = 1e-6;

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
/// `hz`, in steady state.
fn bands_at(freqs: &[f32], hz: f64) -> (Vec<(f64, f64)>, f64) {
    let freqs: Vec<Hz> = freqs.iter().map(|&f| Hz(f)).collect();
    let mut xo = Crossover::new(&freqs, SampleRate::new(RATE)).unwrap();
    let x = sine(hz, RATE, SETTLE + WINDOW);
    let nb = xo.bands();
    let mut bands = vec![Vec::with_capacity(WINDOW); nb];
    let mut sum = Vec::with_capacity(WINDOW);
    for (i, &s) in x.iter().enumerate() {
        let out = xo.split(s);
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

/// The largest sample-to-sample step of `x`.
fn max_step(x: &[f32]) -> f32 {
    x.windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0, f32::max)
}

/// A crossover moved from 800 Hz to 3 kHz mid-signal steps no harder than
/// the steadier of the two settings does.
///
/// The sine is 1 kHz, so the step changes which band it lands in: the
/// low band falls from nearly all of it to almost nothing. What must not
/// appear is a discontinuity beyond that: the largest sample step through
/// the move stays within the steady figure of the louder setting
/// (`2 pi f / fs` of its amplitude) plus 10 %.
#[test]
fn a_frequency_step_is_ramped_not_jumped() {
    let hz = 1_000.0;
    let x = sine(hz, RATE, 24_000);
    let run = |block: usize| {
        let mut xo = Crossover::new(&[Hz(800.0)], SampleRate::new(RATE)).unwrap();
        let mut low = Vec::new();
        let mut high = Vec::new();
        for (k, chunk) in x.chunks(block).enumerate() {
            if k * block == 12_000 / block * block {
                xo.set_frequency(0, Hz(3_000.0)).unwrap();
            }
            let mut out = vec![0.0; 2 * chunk.len()];
            xo.split_block(chunk, &mut out);
            low.extend_from_slice(&out[..chunk.len()]);
            high.extend_from_slice(&out[chunk.len()..]);
        }
        (low, high)
    };
    let (low, high) = run(256);
    // Steady figures, on the second half of each side of the step.
    let steady = |b: &[f32]| max_step(&b[4_000..10_000]).max(max_step(&b[20_000..]));
    let bound = 1.1 * steady(&low).max(steady(&high));
    let moving = |b: &[f32]| max_step(&b[11_000..16_000]);
    assert!(moving(&low) <= bound, "low band stepped {}", moving(&low));
    assert!(
        moving(&high) <= bound,
        "high band stepped {}",
        moving(&high)
    );
}

fn xo(freqs: &[f32]) -> Result<Crossover, CrossoverError> {
    let f: Vec<Hz> = freqs.iter().map(|&f| Hz(f)).collect();
    Crossover::new(&f, SampleRate::new(RATE))
}

/// The band count is 2 to 6.
#[test]
fn the_band_count_is_two_to_six() {
    assert!(matches!(
        xo(&[]),
        Err(CrossoverError::Bands {
            bands: 1,
            min: 2,
            max: 6
        })
    ));
    assert!(xo(&[100.0]).is_ok());
    assert!(xo(&[1.0, 2.0, 3.0, 4.0, 5.0]).is_ok());
    assert!(matches!(
        xo(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]),
        Err(CrossoverError::Bands { bands: 7, .. })
    ));
}

/// Frequencies must be finite, above 0, strictly increasing, below Nyquist.
#[test]
fn frequencies_are_validated() {
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.0, -5.0] {
        assert!(
            matches!(xo(&[bad]), Err(CrossoverError::Frequency { index: 0, .. })),
            "{bad}"
        );
        assert!(
            matches!(
                xo(&[100.0, bad]),
                Err(CrossoverError::Frequency { index: 1, .. })
            ),
            "{bad}"
        );
    }
    assert_eq!(
        xo(&[500.0, 500.0]).unwrap_err(),
        CrossoverError::Order { index: 1 }
    );
    assert_eq!(
        xo(&[500.0, 100.0]).unwrap_err(),
        CrossoverError::Order { index: 1 }
    );
    assert!(matches!(
        xo(&[24_000.0]),
        Err(CrossoverError::Nyquist { index: 0, .. })
    ));
    assert!(matches!(
        xo(&[100.0, 30_000.0]),
        Err(CrossoverError::Nyquist { index: 1, .. })
    ));
    assert!(xo(&[23_999.0]).is_ok());
    for rate in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        assert_eq!(
            Crossover::new(&[Hz(100.0)], SampleRate::new(rate)).unwrap_err(),
            CrossoverError::SampleRate
        );
    }
}

/// A refused `set_frequency` changes nothing; an accepted one is read back.
#[test]
fn set_frequency_validates_against_the_neighbours() {
    let mut c = xo(&[100.0, 1_000.0, 5_000.0]).unwrap();
    for (i, hz) in [
        (0, f32::NAN),
        (1, f32::INFINITY),
        (1, 100.0),
        (1, 5_000.0),
        (0, 1_000.0),
        (2, 1_000.0),
        (2, 24_000.0),
        (2, -1.0),
    ] {
        assert!(c.set_frequency(i, Hz(hz)).is_err(), "{i} {hz}");
    }
    assert_eq!(
        c.set_frequency(3, Hz(9_000.0)),
        Err(CrossoverError::Index { index: 3, len: 3 })
    );
    assert_eq!(
        [
            c.frequency(0),
            c.frequency(1),
            c.frequency(2),
            c.frequency(3)
        ],
        [Some(Hz(100.0)), Some(Hz(1_000.0)), Some(Hz(5_000.0)), None]
    );
    c.set_frequency(1, Hz(1_500.0)).unwrap();
    assert_eq!(c.frequency(1), Some(Hz(1_500.0)));
    assert!(c.set_frequencies(&[Hz(100.0), Hz(200.0)]).is_err());
    assert!(
        c.set_frequencies(&[Hz(100.0), Hz(90.0), Hz(300.0)])
            .is_err()
    );
    assert_eq!(c.frequency(1), Some(Hz(1_500.0)));
}

/// A rate that is not finite and positive changes nothing; a frequency a
/// lower rate leaves above Nyquist still solves to a finite, stable filter.
///
/// The bound: LR4 has no gain peaking, so a unit sine leaves each band at
/// most 1 in steady state; the cascaded 2nd-order Butterworth step
/// response overshoots by under 25 %, so 1.5 leaves margin for the
/// transient and still fails a filter that has gone unstable.
#[test]
fn a_rate_change_keeps_the_filter_finite() {
    let mut c = xo(&[100.0, 20_000.0]).unwrap();
    c.set_sample_rate(SampleRate::new(f64::NAN));
    assert_eq!(c.sample_rate(), SampleRate(RATE));
    c.set_sample_rate(SampleRate::new(8_000.0));
    let mut peak = 0.0f32;
    for x in sine(300.0, 8_000.0, 4_000) {
        for b in c.split(x) {
            assert!(b.is_finite());
            peak = peak.max(b.abs());
        }
    }
    assert!(peak < 1.5, "{peak}");
}

/// `reset` clears the state: after it, silence in is silence out.
#[test]
fn reset_clears_every_section() {
    let mut c = xo(&[100.0, 1_000.0, 5_000.0]).unwrap();
    for x in sine(80.0, RATE, 2_000) {
        c.split(x);
    }
    c.reset();
    assert_eq!(c.split(0.0), [0.0; Crossover::MAX_BANDS]);
}

/// The messages say what is wrong, including a whole set of the wrong length.
#[test]
fn errors_name_what_is_wrong() {
    let e = xo(&[500.0, 100.0]).unwrap_err().to_string();
    assert!(e.contains("crossover 1") && e.contains("increase"), "{e}");
    let mut c = xo(&[100.0, 200.0, 300.0]).unwrap();
    let e = c.set_frequencies(&[Hz(1.0)]).unwrap_err();
    assert_eq!(
        e,
        CrossoverError::Count {
            got: 1,
            expected: 3
        }
    );
    assert_eq!(e.to_string(), "1 frequencies given for 3 crossovers");
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

/// While four bands' frequencies all ramp, the sum keeps unit amplitude.
///
/// A unit 1 kHz tone through `[120, 1000, 6000]` moving to
/// `[150, 1300, 5000]` across 256-frame blocks. The sampled peak of a
/// 48-samples-per-period sine is at least `cos(pi / 48)`, 2.1e-3 under 1,
/// so the bound is 3e-3 (measured: 2e-4 under 1 on the sampled peak, so
/// the ramp's own deviation is under that).
#[test]
fn the_sum_stays_flat_while_frequencies_ramp() {
    let mut c = xo(&[120.0, 1_000.0, 6_000.0]).unwrap();
    let x = sine(1_000.0, RATE, SETTLE + 4_096);
    let mut sum = Vec::new();
    for (k, chunk) in x.chunks(256).enumerate() {
        if k * 256 == SETTLE {
            c.set_frequencies(&[Hz(150.0), Hz(1_300.0), Hz(5_000.0)])
                .unwrap();
        }
        let mut out = vec![0.0; 4 * chunk.len()];
        c.split_block(chunk, &mut out);
        for i in 0..chunk.len() {
            sum.push((0..4).map(|b| out[b * chunk.len() + i]).sum::<f32>());
        }
    }
    // The first 1 kHz period after the move starts to the last of the ramp.
    let peak = sum[SETTLE..SETTLE + 1_024]
        .iter()
        .fold(0.0f32, |m, v| m.max(v.abs()));
    assert!((peak - 1.0).abs() < 3e-3, "peak {peak}");
}
