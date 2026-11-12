use super::*;
use crate::filter::ladder::kernel::LadderCoefficients;
use crate::{Complex32, RealFft};

const RATE: SampleRate = SampleRate(48_000.0);

fn model(ty: LadderType, fc: f32, res: f32) -> LadderModel {
    LadderModel::new(
        LadderControls {
            ty,
            freq: Hz(fc),
            res: Resonance(res),
            drive: Drive::UNITY,
        },
        RATE,
    )
}

fn coefficients(freq: Hz, res: Resonance) -> LadderCoefficients<f64> {
    LadderCoefficients::from(&LadderCoeffs::new(freq, res, RATE))
}

fn at(m: &LadderModel, f: f32) -> Complex64 {
    let mut g = [Complex64::new(0.0, 0.0)];
    m.frequency_response(0, &[Hz(f)], &mut g);
    g[0]
}

/// The closed forms of the linearisation: DC is `1 / (1 + k)` for a low-pass
/// and 0 for a high-pass, the stages are 1/sqrt(2) at the cutoff, so with no
/// resonance a 12 dB type is 1/2 there and a 24 dB type 1/4 (the stage is a
/// trapezoidal one-pole, prewarped to put its corner exactly on the cutoff), and
/// Nyquist is 0 for a low-pass and 1 for a high-pass.
#[test]
fn the_response_has_the_closed_forms() {
    for res in [0.0f32, 0.5, 0.9] {
        let k = 4.0 * f64::from(res);
        let lp = model(LadderType::LP24, 1_000.0, res);
        assert!(
            (at(&lp, 0.0).norm() - 1.0 / (1.0 + k)).abs() < 1e-12,
            "res {res}"
        );
        assert!(at(&lp, 24_000.0).norm() < 1e-12);
        let hp = model(LadderType::HP12, 1_000.0, res);
        assert!(at(&hp, 0.0).norm() < 1e-12);
        assert!((at(&hp, 24_000.0).norm() - 1.0).abs() < 1e-9, "res {res}");
    }
    for (ty, want) in [
        (LadderType::LP12, 0.5),
        (LadderType::LP24, 0.25),
        (LadderType::HP12, 0.5),
        (LadderType::HP24, 0.25),
    ] {
        let g = at(&model(ty, 1_000.0, 0.0), 1_000.0).norm();
        assert!((g - want).abs() < 1e-9, "{ty:?}: {g}");
    }
}

/// The ladder's response, **at small signal**, is the model's: an impulse of
/// 1e-7 through `ladder_step` (where `tanh x = x` to 1e-14) has the model's
/// spectrum to 1e-6 of its norm, for each type and three resonances.
#[test]
fn the_small_signal_response_is_the_models() {
    const N: usize = 1 << 15;
    let amp = 1e-7f64;
    for ty in [
        LadderType::LP12,
        LadderType::LP24,
        LadderType::HP12,
        LadderType::HP24,
    ] {
        for res in [0.0f32, 0.4, 0.8] {
            let m = model(ty, 1_200.0, res);
            let c = coefficients(Hz(1_200.0), Resonance(res));
            let mut s = [0.0f64; 4];
            let ir: Vec<f32> = (0..N)
                .map(|n| (c.step(ty, &mut s, if n == 0 { amp } else { 0.0 }, 1.0) / amp) as f32)
                .collect();
            let fft = RealFft::new(N);
            let mut scratch = fft.new_scratch();
            let mut bins = vec![Complex32::new(0.0, 0.0); fft.bins()];
            fft.forward(&ir, &mut bins, &mut scratch);
            let (mut num, mut den) = (0.0f64, 0.0f64);
            for (i, b) in bins.iter().enumerate().step_by(7) {
                let want = at(&m, (i as f64 * RATE.get() / N as f64) as f32);
                let got = Complex64::new(f64::from(b.re), f64::from(b.im));
                num += (got - want).norm_sqr();
                den += want.norm_sqr();
            }
            let err = (num / den).sqrt();
            assert!(err < 1e-4, "{ty:?} res {res}: {err:.2e}");
        }
    }
}

/// The linearisation is where the model stops: a loud impulse through the
/// real ladder at a high resonance is not the model's response (the `tanh`
/// limits the loop), which is what keeps the class honest. An impulse of 3
/// through a resonance of 0.9 differs from the small-signal one by more than
/// 10% of the response's norm.
#[test]
fn the_model_is_the_small_signal_system_and_loud_signals_leave_it() {
    const N: usize = 1 << 14;
    let c = coefficients(Hz(1_200.0), Resonance(0.9));
    let run = |amp: f64| {
        let mut s = [0.0f64; 4];
        (0..N)
            .map(|n| {
                c.step(
                    LadderType::LP24,
                    &mut s,
                    if n == 0 { amp } else { 0.0 },
                    1.0,
                ) / amp
            })
            .collect::<Vec<f64>>()
    };
    let (small, loud) = (run(1e-7), run(3.0));
    let diff: f64 = small.iter().zip(&loud).map(|(a, b)| (a - b).powi(2)).sum();
    let norm: f64 = small.iter().map(|a| a * a).sum();
    assert!((diff / norm).sqrt() > 0.1, "{}", (diff / norm).sqrt());
}

/// The ring: the slowest pole of the linearisation against the measured decay
/// of the small-signal impulse response, and the tail against the measured time
/// to -60 dB (covers it, within 3 times).
#[test]
fn the_radius_and_the_tail_are_what_the_linearised_filter_rings() {
    const N: usize = 1 << 17;
    let amp = 1e-7f64;
    let m = model(LadderType::LP24, 300.0, 0.8);
    let c = coefficients(Hz(300.0), Resonance(0.8));
    let mut s = [0.0f64; 4];
    let ir: Vec<f64> = (0..N)
        .map(|n| {
            c.step(
                LadderType::LP24,
                &mut s,
                if n == 0 { amp } else { 0.0 },
                1.0,
            ) / amp
        })
        .collect();
    let r = m.spectral_radius();
    assert!(r < 1.0 && r > 0.99, "{r}");
    // The envelope over windows of 256 frames: the energy decays as r^(2 n).
    let w = 256;
    let e: Vec<f64> = ir
        .chunks(w)
        .map(|c| c.iter().map(|x| x * x).sum::<f64>())
        .collect();
    let top = e.iter().copied().fold(0.0, f64::max);
    let pts: Vec<(f64, f64)> = e
        .iter()
        .enumerate()
        .filter(|(_, v)| **v < top * 1e-3 && **v > top * 1e-9)
        .map(|(i, v)| (i as f64, libm::log(*v)))
        .collect();
    let n = pts.len() as f64;
    assert!(pts.len() > 5, "{} points, top {top}", pts.len());
    let (sx, sy) = pts.iter().fold((0.0, 0.0), |(a, b), p| (a + p.0, b + p.1));
    let sxx: f64 = pts.iter().map(|p| p.0 * p.0).sum();
    let sxy: f64 = pts.iter().map(|p| p.0 * p.1).sum();
    let measured = -((n * sxy - sx * sy) / (n * sxx - sx * sx)) / (2.0 * w as f64);
    let bound = -libm::log(r);
    assert!(
        measured >= 0.97 * bound && measured <= 1.1 * bound,
        "measured {measured:.3e}, the radius says {bound:.3e}"
    );
    // Time to -60 dB of the peak envelope, in 256-frame windows.
    let win = 256;
    let env: Vec<f64> = ir
        .chunks(win)
        .map(|c| (c.iter().map(|x| x * x).sum::<f64>() / win as f64).sqrt())
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
}

/// A full resonance (`k = 4`) puts the linearisation's poles on the unit circle:
/// `is_stable` is false, the tail unbounded, and the real filter is held by the
/// saturator. (Where the poles sit is `the_poles_cross_the_unit_circle_at_k_four`.)
#[test]
fn a_full_resonance_is_unstable_at_small_signal() {
    assert!(!model(LadderType::LP24, 1_000.0, 1.0).is_stable());
    assert!(model(LadderType::LP24, 1_000.0, 0.5).is_stable());
    assert!(
        model(LadderType::LP24, 1_000.0, 0.5)
            .tail(Db(-60.0))
            .seconds()
            .is_some()
    );
    assert_eq!(
        model(LadderType::LP24, 1_000.0, 1.0).tail(Db(-60.0)),
        Ring::Unbounded
    );
}

/// The spec: the LP24 model is the continuous ladder `1 / ((1 + s/wc)^4 + k)`,
/// `k = 4 res`, under the bilinear map prewarped at the cutoff,
/// `s/wc = (1/g)(1 - z^-1)/(1 + z^-1)` with `g = tan(pi fc / rate)`, evaluated
/// directly. Resonances 0, 0.5 and 0.9 (a peak at 975 Hz for 1 kHz, not the delayed
/// loop's 958 Hz), to 1e-9 relative.
#[test]
fn the_lp24_model_is_the_prewarped_bilinear_image_of_the_continuous_ladder() {
    for (fc, res) in [
        (1_000.0f32, 0.0f32),
        (1_000.0, 0.5),
        (1_000.0, 0.9),
        (4_000.0, 0.9),
    ] {
        let m = model(LadderType::LP24, fc, res);
        let g = (PI * f64::from(fc) / RATE.get()).tan();
        let k = 4.0 * f64::from(res);
        for f in [
            50.0f32, 500.0, 900.0, 975.0, 1_000.0, 2_000.0, 3_900.0, 9_000.0,
        ] {
            let w = 2.0 * PI * f64::from(f) / RATE.get();
            let zi = Complex64::from_polar(1.0, -w);
            let s_over_wc = (1.0 - zi) / (1.0 + zi) / g;
            let one = Complex64::new(1.0, 0.0);
            let p = one + s_over_wc;
            let want = one / (p * p * p * p + k);
            let got = at(&m, f);
            let rel = (got - want).norm() / want.norm();
            assert!(rel < 1e-9, "fc {fc} res {res} at {f} Hz: {rel:.2e}");
        }
    }
}

/// The continuous ladder self-oscillates at `k = 4` and not below: the model's
/// poles are inside the unit circle for `res < 1` and the loop reaches it at 1.
#[test]
fn the_poles_cross_the_unit_circle_at_k_four() {
    // The measured poles agree with the closed form: inside the circle below
    // `k = 4`, on it at `k = 4` (the root iteration's rounding is the tolerance).
    for res in [0.9f32, 0.99, 0.999] {
        let m = model(LadderType::LP24, 1_000.0, res);
        assert!(m.is_stable(), "{res}");
        assert!(m.spectral_radius() < 1.0, "{res}: {}", m.spectral_radius());
    }
    let m = model(LadderType::LP24, 1_000.0, 1.0);
    assert!(!m.is_stable());
    let r = m.spectral_radius();
    assert!((r - 1.0).abs() < 1e-9, "{r}");
}
