//! The SVF's magnitude response against the RBJ Audio EQ Cookbook's closed form.
//!
//! # The oracle
//!
//! Robert Bristow-Johnson's *Audio EQ Cookbook* gives the low-pass, high-pass
//! and band-pass biquads as closed-form coefficients of `f0`, `Q` and `fs`.
//! [`cookbook_gain_db`] builds those coefficients and evaluates
//! `|H(e^{j*omega})|` analytically in `f64` at each probe frequency. No filter
//! runs on the oracle side.
//!
//! The band-pass is the cookbook's constant-skirt-gain form, whose peak gain at
//! `f0` is `Q`; that is the form `SvfType::BandPass` documents.
//!
//! # Why the two agree
//!
//! The cookbook biquads are the bilinear transform, with the cutoff prewarped,
//! of the second-order analog prototype `1 / (s^2 + s/Q + 1)` (and its
//! high-pass and band-pass siblings). The TPT state-variable filter applies the
//! same transform to the same prototype, `g = tan(pi*f0/fs)` being the
//! prewarp. The two digital transfer functions are therefore identical and
//! differ only in how the state is stored.
//!
//! # Tolerance
//!
//! 0.01 dB. The measurement is an RMS ratio of `f32` samples over a settled
//! window, so its noise floor is far below that, while a 1% cutoff error moves
//! the response by more than the bound at the probes near the corner.

use tutti_nodes::{SvfFilterNode, SvfType};

const SR: f64 = 48_000.0;
const Q: f32 = 0.707;
const CUTOFF: f32 = 1_000.0;

/// Steady-state gain of `f` at `freq`, measured as an RMS ratio.
///
/// Drives a sine long enough for the transient to settle, discards the first
/// half, and compares output RMS to input RMS. Measuring rather than evaluating
/// the transfer function is deliberate: it tests the filter that actually runs,
/// including its coefficient update path, not a formula restated in the test.
fn measured_gain_db(mut render: impl FnMut(f32) -> f32, freq: f32) -> f64 {
    let n = 24_000usize; // 0.5 s at 48 kHz — plenty for a 12 dB/oct settle.
    let mut input = Vec::with_capacity(n);
    let mut output = Vec::with_capacity(n);
    for i in 0..n {
        let x = (std::f64::consts::TAU * freq as f64 * i as f64 / SR).sin() as f32;
        input.push(x);
        output.push(render(x));
    }
    // Second half only: the first half carries the startup transient.
    let rms = |v: &[f32]| -> f64 {
        let s = &v[v.len() / 2..];
        (s.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>() / s.len() as f64).sqrt()
    };
    20.0 * (rms(&output) / rms(&input)).log10()
}

/// Drive tutti's SVF one sample at a time, as a graph node prepared at `SR`.
fn svf_gain_db(kind: SvfType, cutoff: f32, freq: f32) -> f64 {
    let rate = tutti_types::SampleRate::of(SR);
    let mut node = tutti_graph::contract::prepared(
        SvfFilterNode::<f64>::new(kind, tutti_types::Hz::new(cutoff), tutti_types::Q::new(Q)),
        rate,
        1,
    );
    measured_gain_db(
        move |x| tutti_graph::contract::drive(&mut node, rate, &[&[x]], &[])[0][0],
        freq,
    )
}

/// The cookbook's biquad for `kind`, evaluated at `freq`: `|H(e^{j*omega})|` in dB.
///
/// Coefficients: `w0 = 2*pi*f0/fs`, `alpha = sin(w0)/(2*Q)`, then the
/// low-pass, high-pass and constant-skirt-gain band-pass numerators over
/// `a0 = 1 + alpha`, `a1 = -2*cos(w0)`, `a2 = 1 - alpha`.
fn cookbook_gain_db(kind: SvfType, f0: f64, freq: f64) -> f64 {
    let w0 = std::f64::consts::TAU * f0 / SR;
    let alpha = w0.sin() / (2.0 * Q as f64);
    let cs = w0.cos();
    let b = match kind {
        SvfType::LowPass => [(1.0 - cs) / 2.0, 1.0 - cs, (1.0 - cs) / 2.0],
        SvfType::HighPass => [(1.0 + cs) / 2.0, -(1.0 + cs), (1.0 + cs) / 2.0],
        SvfType::BandPass => [w0.sin() / 2.0, 0.0, -w0.sin() / 2.0],
        other => panic!("the cookbook oracle covers LPF, HPF and BPF, not {other:?}"),
    };
    let a = [1.0 + alpha, -2.0 * cs, 1.0 - alpha];

    // H(z) = (b0 + b1 z^-1 + b2 z^-2) / (a0 + a1 z^-1 + a2 z^-2) at z = e^{j*omega}.
    let omega = std::f64::consts::TAU * freq / SR;
    let poly = |c: [f64; 3]| -> (f64, f64) {
        let (mut re, mut im) = (0.0, 0.0);
        for (k, ck) in c.iter().enumerate() {
            re += ck * (omega * k as f64).cos();
            im -= ck * (omega * k as f64).sin();
        }
        (re, im)
    };
    let (nr, ni) = poly(b);
    let (dr, di) = poly(a);
    10.0 * ((nr * nr + ni * ni) / (dr * dr + di * di)).log10()
}

/// Five frequencies spanning two decades around the 1 kHz corner.
const PROBES: [f32; 5] = [100.0, 500.0, 1_000.0, 2_000.0, 10_000.0];

/// Largest |measured - analytic| in dB over [`PROBES`] for `kind`.
fn worst_delta_db(label: &str, kind: SvfType) -> f64 {
    let mut worst = 0.0f64;
    for f in PROBES {
        let measured = svf_gain_db(kind, CUTOFF, f);
        let analytic = cookbook_gain_db(kind, CUTOFF as f64, f as f64);
        let d = (measured - analytic).abs();
        println!(
            "{label} {f:>8} Hz: svf {measured:>8.3} dB, cookbook {analytic:>8.3} dB, delta {d:.4} dB"
        );
        worst = worst.max(d);
    }
    worst
}

#[test]
fn svf_lowpass_matches_the_rbj_cookbook_response() {
    let worst = worst_delta_db("LP", SvfType::LowPass);
    assert!(
        worst < 0.01,
        "LP: worst deviation {worst:.4} dB exceeds 0.01 dB"
    );
}

#[test]
fn svf_highpass_matches_the_rbj_cookbook_response() {
    let worst = worst_delta_db("HP", SvfType::HighPass);
    assert!(
        worst < 0.01,
        "HP: worst deviation {worst:.4} dB exceeds 0.01 dB"
    );
}

#[test]
fn svf_bandpass_matches_the_rbj_constant_skirt_response() {
    let worst = worst_delta_db("BP", SvfType::BandPass);
    assert!(
        worst < 0.01,
        "BP: worst deviation {worst:.4} dB exceeds 0.01 dB"
    );
}

/// The fixed points of the closed form, measured on the running SVF.
const HALF_POWER_DB: f64 = -3.0103;

/// At `fc` the cookbook's low-pass and high-pass have magnitude `Q`, which at
/// `Q = 1/sqrt(2)` is -3.01 dB.
#[test]
fn svf_lowpass_and_highpass_are_minus_3_db_at_the_cutoff_for_butterworth_q() {
    for (label, kind) in [("LP", SvfType::LowPass), ("HP", SvfType::HighPass)] {
        let g = svf_gain_db(kind, CUTOFF, CUTOFF);
        assert!(
            (g - HALF_POWER_DB).abs() < 0.01,
            "{label} at fc: {g:.4} dB, expected {HALF_POWER_DB} dB"
        );
    }
}

/// The constant-skirt-gain band-pass peaks at `Q` at `fc`: `20*log10(Q)` dB
/// (0 dB only at `Q = 1`).
#[test]
fn svf_bandpass_peak_at_the_cutoff_is_20_log10_q() {
    let expected = 20.0 * (Q as f64).log10();
    let g = svf_gain_db(SvfType::BandPass, CUTOFF, CUTOFF);
    assert!(
        (g - expected).abs() < 0.01,
        "BP at fc: {g:.4} dB, expected 20*log10(Q) = {expected:.4} dB"
    );
}

/// Well above `fc` the low-pass falls 12 dB per octave, i.e. `40*log10(2)` =
/// 12.04 dB per doubling of frequency.
///
/// The closed form is not exactly that slope at finite `x = f/fc`: the analog
/// prototype's `(1-x^2)^2 + x^2/Q^2` is not `x^4`, and the bilinear transform
/// adds its warp. At `fc = 250 Hz` the probes are 2 and 4 kHz, and the
/// cookbook's own drop between them is the bound's centre: the test asserts the
/// measured drop is within the closed form's distance from -12.04 dB, plus
/// 0.01 dB per probe. The bound is printed, and asserted below 0.5 dB so it
/// cannot grow loose.
#[test]
fn svf_lowpass_falls_12_db_per_octave_above_the_cutoff() {
    let fc = 250.0;
    let (f1, f2) = (8.0 * fc, 16.0 * fc);
    let analytic = cookbook_gain_db(SvfType::LowPass, fc as f64, f2 as f64)
        - cookbook_gain_db(SvfType::LowPass, fc as f64, f1 as f64);
    let slope_db = 40.0 * 2f64.log10();
    let bound = (analytic + slope_db).abs() + 0.02;
    println!("closed-form octave drop {analytic:.4} dB, bound {bound:.4} dB around -{slope_db:.4}");
    let drop = svf_gain_db(SvfType::LowPass, fc, f2) - svf_gain_db(SvfType::LowPass, fc, f1);
    assert!(
        (drop + slope_db).abs() < bound,
        "LP octave drop {drop:.4} dB, expected -{slope_db:.4} dB within {bound:.4} dB"
    );
    assert!(bound < 0.5, "the derived bound {bound:.4} dB is not tight");
}

/// Orientation: the low-pass passes two octaves below `fc` and attenuates two
/// octaves above; the high-pass does the reverse. At `x = 1/4` the closed
/// form is within 0.02 dB of 0 dB; at `x = 4` it is below -20 dB.
#[test]
fn lowpass_passes_below_and_highpass_passes_above_the_cutoff() {
    let (lo, hi) = (CUTOFF / 4.0, CUTOFF * 4.0);
    let lp_lo = svf_gain_db(SvfType::LowPass, CUTOFF, lo);
    let lp_hi = svf_gain_db(SvfType::LowPass, CUTOFF, hi);
    let hp_lo = svf_gain_db(SvfType::HighPass, CUTOFF, lo);
    let hp_hi = svf_gain_db(SvfType::HighPass, CUTOFF, hi);
    assert!(
        lp_lo.abs() < 0.1,
        "LP at fc/4: {lp_lo:.3} dB, expected about 0"
    );
    assert!(
        lp_hi < -20.0,
        "LP at 4 fc: {lp_hi:.3} dB, expected below -20"
    );
    assert!(
        hp_hi.abs() < 0.1,
        "HP at 4 fc: {hp_hi:.3} dB, expected about 0"
    );
    assert!(
        hp_lo < -20.0,
        "HP at fc/4: {hp_lo:.3} dB, expected below -20"
    );
}
