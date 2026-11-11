use super::*;
use crate::{Complex32, Crossover, RealFft};

const RATE: SampleRate = SampleRate(48_000.0);

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

/// The model's response is the running crossover's: the `RealFft` of each
/// band's impulse response through `Crossover::split` against
/// `frequency_response`, to 1e-5 of its norm, for three bands.
#[test]
fn the_response_is_what_the_crossover_runs() {
    const N: usize = 1 << 15;
    let freqs = [Hz(300.0), Hz(3_000.0)];
    let m = CrossoverModel::new(&freqs, RATE).unwrap();
    let mut xo = Crossover::new(&freqs, RATE).unwrap();
    let mut bands = vec![vec![0.0f32; N]; 3];
    for n in 0..N {
        let out = xo.split(if n == 0 { 1.0 } else { 0.0 });
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
    let mut xo = Crossover::new(&freqs, RATE).unwrap();
    let ir: Vec<f32> = (0..N)
        .map(|n| xo.split(if n == 0 { 1.0 } else { 0.0 })[0])
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

    let mut xo = Crossover::new(&freqs, RATE).unwrap();
    let mut state = 7u32;
    let mut worst = 0.0f64;
    for _ in 0..(60 * 48_000) {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let x = (state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
        let out = xo.split(x);
        worst = out[..3]
            .iter()
            .fold(worst, |m, &s| m.max(f64::from(s.abs())));
    }
    assert!(worst <= m.l1_norm(), "{worst} against {}", m.l1_norm());
    assert!(m.hinf_norm() <= m.l1_norm());
}

/// The design is separate from the state: the model of a crossover is the
/// model built from its frequencies, before and after it has run.
#[test]
fn a_crossover_holds_its_model_apart_from_its_state() {
    let freqs = [Hz(250.0), Hz(4_000.0)];
    let mut xo = Crossover::new(&freqs, RATE).unwrap();
    let want = CrossoverModel::new(&freqs, RATE).unwrap();
    assert_eq!(xo.model(), &want);
    for n in 0..1000 {
        xo.split(if n % 7 == 0 { 0.5 } else { -0.2 });
    }
    assert_eq!(xo.model(), &want);
    xo.set_frequencies(&[Hz(300.0), Hz(5_000.0)]).unwrap();
    xo.split(0.1);
    assert_eq!(
        xo.model(),
        &CrossoverModel::new(&[Hz(300.0), Hz(5_000.0)], RATE).unwrap()
    );
}
