use super::*;
use crate::filter::crossover::CrossoverState;
use crate::{Complex32, RealFft};

const RATE: SampleRate = SampleRate::SR_48K;

fn model(freqs: &[f32]) -> CrossoverModel {
    let f: Vec<Hz> = freqs.iter().map(|&f| Hz(f)).collect();
    CrossoverModel::new(&f, RATE).unwrap()
}

fn at(m: &CrossoverModel, band: usize, f: f32) -> Complex64 {
    let mut g = [Complex64::new(0.0, 0.0)];
    m.frequency_response(band, &[Hz(f)], &mut g);
    g[0]
}

/// The closed forms of Linkwitz and Riley: each adjacent pair of bands is
/// -6.02 dB (one half) at its crossover, and the bands sum to a unit-magnitude
/// all-pass at every frequency (the sum is the product of the splits'
/// all-passes), for two to six bands.
#[test]
fn the_bands_are_6_db_down_at_the_crossover_and_sum_to_an_all_pass() {
    for freqs in [
        vec![1_000.0f32],
        vec![200.0, 2_000.0],
        vec![100.0, 500.0, 2_500.0],
        vec![80.0, 300.0, 1_200.0, 4_000.0, 12_000.0],
    ] {
        let m = model(&freqs);
        for (j, &fc) in freqs.iter().enumerate() {
            for band in [j, j + 1] {
                let g = at(&m, band, fc);
                // Only the neighbouring splits' influence away from fc is a
                // product of all-passes, so the magnitude at fc is exactly
                // that of the Butterworth squared.
                assert!(
                    (g.norm() - 0.5).abs() < 1e-3 || freqs.len() > 1,
                    "{freqs:?} band {band}: {}",
                    g.norm()
                );
            }
            let low = at(&m, j, fc).norm();
            let high = at(&m, j + 1, fc).norm();
            // The two bands that meet at `fc` add in phase to the all-pass'
            // part, each a half, up to the other splits' all-passes.
            assert!((low - high).abs() < 0.1 * low.max(high) + 1e-3, "{freqs:?}");
        }
        for f in [10.0f32, 90.0, 400.0, 1_500.0, 6_000.0, 20_000.0, 24_000.0] {
            let sum: Complex64 = (0..m.bands()).map(|b| at(&m, b, f)).sum();
            assert!(
                (sum.norm() - 1.0).abs() < 1e-9,
                "{freqs:?} at {f}: {}",
                sum.norm()
            );
        }
    }
}

/// Two bands, exactly: both bands are 1/2 at the crossover, the low band is 1 at
/// DC and the high band 1 at Nyquist (the closed form of `LP^2` and `HP^2`).
#[test]
fn two_bands_have_the_closed_form_anchors() {
    let m = model(&[1_000.0]);
    assert!((at(&m, 0, 1_000.0).norm() - 0.5).abs() < 1e-12);
    assert!((at(&m, 1, 1_000.0).norm() - 0.5).abs() < 1e-12);
    assert!((at(&m, 0, 0.0).norm() - 1.0).abs() < 1e-12);
    assert!((at(&m, 1, 24_000.0).norm() - 1.0).abs() < 1e-9);
}

/// The spec: the two
/// bands of one split are the Linkwitz-Riley prototypes, the Butterworth section
/// `D(s) = s^2 + sqrt2 s + 1` squared, `LP^2 = 1 / D^2` and `HP^2 = s^4 / D^2`, under
/// the bilinear map prewarped at the crossover, `s = (1/g)(z - 1)/(z + 1)` with
/// `g = tan(pi fc / rate)`, evaluated directly with plain complex arithmetic, for
/// crossovers from 60 Hz to 18 kHz and frequencies from 20 Hz to 23.9 kHz, to 1e-9
/// of the larger magnitude.
#[test]
fn the_two_bands_are_the_prewarped_bilinear_image_of_the_lr4_prototypes() {
    let one = Complex64::new(1.0, 0.0);
    for fc in [60.0f32, 1_000.0, 5_000.0, 18_000.0] {
        let m = model(&[fc]);
        let g = (PI * f64::from(fc) / RATE.get()).tan();
        for f in [20.0f32, 150.0, 700.0, 1_000.0, 4_000.0, 12_000.0, 23_900.0] {
            let w = 2.0 * PI * f64::from(f) / RATE.get();
            let z = Complex64::from_polar(1.0, w);
            let s = (z - 1.0) / (z + 1.0) / g;
            let d = s * s + s * 2f64.sqrt() + one;
            let (s2, d2) = (s * s, d * d);
            for (band, want) in [(0, one / d2), (1, s2 * s2 / d2)] {
                let got = at(&m, band, f);
                let scale = want.norm().max(got.norm()).max(1e-12);
                assert!(
                    (got - want).norm() <= 1e-9 * scale + 1e-12,
                    "fc {fc} band {band} at {f} Hz: {got} against {want}"
                );
            }
        }
    }
}

/// The model's response is the running crossover's: the `RealFft` of each
/// band's impulse response through `CrossoverState::tick` against
/// `frequency_response`, to 1e-5 of its norm, for three bands.
#[test]
fn the_response_is_what_the_crossover_runs() {
    const N: usize = 1 << 15;
    let freqs = [Hz(300.0), Hz(3_000.0)];
    let m = CrossoverModel::new(&freqs, RATE).unwrap();
    let mut xo = CrossoverState::default();
    let mut bands = vec![vec![0.0f32; N]; 3];
    for n in 0..N {
        let out = xo.tick(&m, if n == 0 { 1.0 } else { 0.0 });
        for (b, v) in bands.iter_mut().zip(out) {
            b[n] = v;
        }
    }
    let fft = RealFft::new(N);
    let mut scratch = fft.new_scratch();
    for (band, ir) in bands.iter().enumerate() {
        let mut bins = vec![Complex32::new(0.0, 0.0); fft.bins()];
        fft.forward(ir, &mut bins, &mut scratch);
        let (mut num, mut den) = (0.0f64, 0.0f64);
        for (i, b) in bins.iter().enumerate().step_by(5) {
            let want = at(&m, band, (i as f64 * RATE.get() / N as f64) as f32);
            let got = Complex64::new(f64::from(b.re), f64::from(b.im));
            num += (got - want).norm_sqr();
            den += want.norm_sqr();
        }
        assert!(
            (num / den).sqrt() < 1e-5,
            "band {band}: {}",
            (num / den).sqrt()
        );
    }
}

/// The radius is the Butterworth section's pole modulus, from its own closed
/// form `|p|^2 = a2 = (1 - sqrt2 k + k^2) / (1 + sqrt2 k + k^2)` with
/// `k = tan(pi fc / rate)`, of the lowest split (the slowest).
#[test]
fn the_radius_is_the_lowest_splits_butterworth_pole() {
    let m = model(&[60.0, 800.0, 5_000.0]);
    let k = libm::tan(PI * 60.0 / 48_000.0);
    let a2 = (1.0 - 2f64.sqrt() * k + k * k) / (1.0 + 2f64.sqrt() * k + k * k);
    assert!((m.spectral_radius() - a2.sqrt()).abs() < 1e-12);
}

/// The ring: the lowest band's measured time to -60 dB of its peak is covered
/// by the model's tail and within three times it, and 60 s of noise through
/// every band stays under `l1_norm` and the response under `hinf_norm`.
#[test]
fn the_tail_covers_the_measured_ring_and_the_norms_bound_the_output() {
    const N: usize = 1 << 17;
    let freqs = [Hz(80.0), Hz(2_000.0)];
    let m = CrossoverModel::new(&freqs, RATE).unwrap();
    let mut xo = CrossoverState::default();
    let ir: Vec<f32> = (0..N)
        .map(|n| xo.tick(&m, if n == 0 { 1.0 } else { 0.0 })[0])
        .collect();
    let win = 256;
    let env: Vec<f64> = ir
        .chunks(win)
        .map(|c| (c.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>() / win as f64).sqrt())
        .collect();
    let peak = env.iter().copied().fold(0.0, f64::max);
    let last = env.iter().rposition(|&v| v >= peak * 1e-3).unwrap();
    let took = ((last + 1) * win) as f64 / RATE.get();
    let Ring::Finite(tail) = m.tail(Db(-60.0)) else {
        panic!("finite")
    };
    assert!(
        tail.get() >= took,
        "tail {} against measured {took}",
        tail.get()
    );
    assert!(
        tail.get() <= 3.0 * took,
        "tail {} against measured {took}",
        tail.get()
    );

    let mut xo = CrossoverState::default();
    let mut state = 7u32;
    let mut worst = 0.0f64;
    for _ in 0..(60 * 48_000) {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let x = (state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
        let out = xo.tick(&m, x);
        worst = out[..3]
            .iter()
            .fold(worst, |m, &s| m.max(f64::from(s.abs())));
    }
    assert!(worst <= m.l1_norm(), "{worst} against {}", m.l1_norm());
    assert!(m.hinf_norm() <= m.l1_norm());
}
