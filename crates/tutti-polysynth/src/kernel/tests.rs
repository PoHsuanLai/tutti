use super::*;

/// *Mutation:* dropping the `x^9` term of the polynomial fails this.
#[test]
fn sine_matches_libm_over_a_cycle() {
    for i in 0..4096 {
        let p = i as f32 / 4096.0;
        let got = sine(V::splat(p)).to_array()[0];
        let want = (core::f64::consts::TAU * f64::from(p)).sin() as f32;
        assert!(
            (got - want).abs() < 2e-6,
            "sin(2π·{p}) = {got}, libm {want}"
        );
    }
}

/// *Mutation:* removing the argument clamp lets the approximant pass
/// unity past `|x| = 4.97` and fails the overshoot check.
#[test]
fn tanh_is_close_to_libm_and_never_overshoots() {
    let mut worst = 0.0f32;
    for i in -8000..=8000 {
        let x = i as f32 / 1000.0;
        let got = tanh(V::splat(x)).to_array()[0];
        let want = f64::from(x).tanh() as f32;
        worst = worst.max((got - want).abs());
        assert!(got.abs() <= 1.0, "tanh({x}) = {got} overshoots unity");
        if x.abs() < 3.0 {
            assert!((got - want).abs() < 1e-5, "tanh({x}) = {got}, libm {want}");
        }
    }
    assert!(worst < 3e-4, "worst tanh error {worst}");
}

/// *Mutation:* offsetting the float mapping by 0.1 (`- 2.9`) fails both
/// the range and the mean check.
#[test]
fn white_noise_is_in_range_and_centred() {
    let mut state = u32x8::new(core::array::from_fn(|k| 0x9E37_79B9 ^ (k as u32 + 1)));
    let (mut sum, mut n) = (0.0f64, 0usize);
    for _ in 0..10_000 {
        for s in white(&mut state).to_array() {
            assert!((-1.0..1.0).contains(&s), "white sample {s} out of range");
            sum += f64::from(s);
            n += 1;
        }
    }
    assert!(
        (sum / n as f64).abs() < 0.01,
        "white noise mean {}",
        sum / n as f64
    );
}

/// Sample rate and pitch for the aliasing measurements: 2990 Hz is exactly
/// 299 cycles in 4410 samples, so every harmonic *and every alias* lands on
/// an integer DFT bin — and since 4410 is not a multiple of 299, no alias
/// lands on a harmonic's bin.
const SR: f64 = 44_100.0;
const F0: f64 = 2_990.0;
const N: usize = 4_410;
const CYCLES: usize = 299;

/// Render `N` samples of `osc` at `F0`.
fn render(osc: impl Fn(V, V, V) -> V) -> Vec<f64> {
    let inc = (F0 / SR) as f32;
    let (dt, inv) = (V::splat(inc), V::splat(1.0 / inc));
    let mut phase = V::ZERO;
    (0..N)
        .map(|_| {
            let y = osc(phase, dt, inv).to_array()[0];
            phase = wrap_once(phase + dt);
            f64::from(y)
        })
        .collect()
}

/// Fraction of the signal's AC power that is *not* at a harmonic of `F0` —
/// i.e. aliasing, since a periodic waveform has nothing else.
fn inharmonic_fraction(x: &[f64]) -> f64 {
    let n = x.len() as f64;
    let mean = x.iter().sum::<f64>() / n;
    let total: f64 = x.iter().map(|s| (s - mean).powi(2)).sum::<f64>() / n;
    let mut harmonic = 0.0;
    let mut k = CYCLES;
    while k < N / 2 {
        let (mut re, mut im) = (0.0, 0.0);
        for (i, s) in x.iter().enumerate() {
            let a = core::f64::consts::TAU * (k * i % N) as f64 / n;
            re += s * a.cos();
            im -= s * a.sin();
        }
        harmonic += 2.0 * (re * re + im * im) / (n * n);
        k += CYCLES;
    }
    (total - harmonic).max(0.0) / total
}

fn naive_saw(p: V, _: V, _: V) -> V {
    p + p - V::ONE
}

fn naive_pulse(p: V, _: V, _: V) -> V {
    p.simd_lt(V::splat(0.5)).select(V::ONE, -V::ONE)
}

fn naive_triangle(p: V, _: V, _: V) -> V {
    let q = wrap_once(p + V::splat(0.25));
    V::ONE - V::splat(4.0) * (q - V::splat(0.5)).abs()
}

fn square(p: V, dt: V, inv: V) -> V {
    pulse(p, V::splat(0.5), dt, inv)
}

/// The band-limited waveforms must alias far less than their naive
/// counterparts at a pitch high enough for aliasing to matter.
///
/// Measured at 2990 Hz / 44.1 kHz, where the naive saw folds 8% of its AC
/// power back as inharmonic partials — an audible rasp. Measured margins:
/// saw 15.8 dB, pulse 18.3 dB, triangle 15.1 dB (its naive aliasing is
/// already low, since its harmonics fall at 12 dB/octave).
///
/// *Mutations, each run:* dropping the `poly_blep` term from `saw` fails
/// the saw case (0 dB); dropping the falling-edge correction from `pulse`
/// fails the pulse case; scaling the triangle's BLAMP by `8·dt` — the full
/// slope change, the natural misreading of the residual's normalization —
/// fails the triangle case at 2.3 dB.
#[test]
fn band_limited_oscillators_alias_far_less_than_naive() {
    type Osc = fn(V, V, V) -> V;
    let cases: [(&str, Osc, Osc, f64); 3] = [
        ("saw", saw, naive_saw, 12.0),
        ("pulse", square, naive_pulse, 12.0),
        ("triangle", triangle, naive_triangle, 12.0),
    ];
    for (name, bl, naive, min_db) in cases {
        let a_bl = inharmonic_fraction(&render(bl));
        let a_naive = inharmonic_fraction(&render(naive));
        let improvement_db = 10.0 * (a_naive / a_bl).log10();
        assert!(
            improvement_db > min_db,
            "{name}: band-limited aliasing {a_bl:.2e} vs naive {a_naive:.2e} — \
             only {improvement_db:.1} dB better, want > {min_db} dB"
        );
    }
}

/// The band-limited waveforms keep the level of the naive ones: the
/// correction only touches the samples next to a corner or edge.
///
/// *Mutation:* flipping the sign of the residual in `saw` (`+ poly_blep`)
/// doubles the edge instead of rounding it and fails here.
#[test]
fn band_limited_oscillators_keep_the_naive_level() {
    type Osc = fn(V, V, V) -> V;
    let cases: [(&str, Osc, Osc); 3] = [
        ("saw", saw, naive_saw),
        ("pulse", square, naive_pulse),
        ("triangle", triangle, naive_triangle),
    ];
    let rms = |x: &[f64]| (x.iter().map(|s| s * s).sum::<f64>() / x.len() as f64).sqrt();
    for (name, bl, naive) in cases {
        let (a, b) = (rms(&render(bl)), rms(&render(naive)));
        assert!(
            (a / b - 1.0).abs() < 0.1,
            "{name}: band-limited RMS {a:.3} vs naive {b:.3}"
        );
    }
}
