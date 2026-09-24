//! Per-sample DSP primitives, each evaluated across eight voice lanes at once.
//!
//! Everything here is a pure function of `f32x8` lanes: no state, no branches
//! on lane values (a lane-dependent branch is a `select`), no allocation. The
//! state these functions advance lives in `crate::bank`, which is the only
//! caller.
//!
//! These are *audio-rate* generators. `tutti-mod`'s `Lfo` shapes are
//! control-rate and not band-limited, which is why the synth does not reuse
//! them: a naive saw at 4 kHz folds most of its energy back below Nyquist as
//! inharmonic partials.

use wide::{f32x8, u32x8};

/// Eight voice lanes of `f32`.
pub(crate) type V = f32x8;

/// Lanes per [`V`].
pub(crate) const LANES: usize = 8;

/// The largest phase increment the band-limiting corrections support.
///
/// PolyBLEP and PolyBLAMP correct the samples within one increment either side
/// of a discontinuity. Past half a cycle per sample those two regions overlap
/// and the correction double-counts, so a pitch above Nyquist is clamped just
/// below it rather than rendered as garbage.
pub(crate) const MAX_INCREMENT: f32 = 0.49;

/// `sin(2π·p)` for a phase `p` in turns, `0.0..1.0`.
///
/// Folds the phase into the quarter-wave around zero and evaluates an odd
/// Taylor polynomial to the 11th power there. On `|x| <= π/2` the first
/// omitted term is `(π/2)^13 / 13! ≈ 5.7e-8`, below `f32` resolution at unity,
/// so this is exact to within rounding. A vector `sin` from a libm would buy
/// nothing but a range reduction the phase representation makes unnecessary.
#[inline(always)]
pub(crate) fn sine(p: V) -> V {
    let half = V::splat(0.5);
    let quarter = V::splat(0.25);
    // sin(2π·p) = -sin(2π·x) with x = p - 0.5 in [-0.5, 0.5).
    let x = p - half;
    // Mirror about ±0.25 so |x| <= 0.25: sin(π - a) = sin(a).
    let x = x.simd_gt(quarter).select(half - x, x);
    let x = x.simd_lt(-quarter).select(-half - x, x);
    let y = x * V::splat(core::f32::consts::TAU);
    let y2 = y * y;
    let poly = V::splat(-1.0 / 39_916_800.0);
    let poly = poly.mul_add(y2, V::splat(1.0 / 362_880.0));
    let poly = poly.mul_add(y2, V::splat(-1.0 / 5_040.0));
    let poly = poly.mul_add(y2, V::splat(1.0 / 120.0));
    let poly = poly.mul_add(y2, V::splat(-1.0 / 6.0));
    let poly = poly.mul_add(y2, V::ONE);
    -(y * poly)
}

/// The two-sample polynomial band-limited step residual (Välimäki, 2007).
///
/// `t` is the phase in turns and `dt` the increment; the result is the
/// correction for a unit **upward** step at `t = 0`, scaled so that adding it
/// to a naive `-1 → +1` edge (a step of 2) rounds that edge. Nonzero only
/// within one increment of the wrap.
#[inline(always)]
pub(crate) fn poly_blep(t: V, dt: V, inv_dt: V) -> V {
    // Just after the step: x in [0, 1), residual 2x - x² - 1.
    let x0 = t * inv_dt;
    let after = x0 + x0 - x0 * x0 - V::ONE;
    // Just before it: x in (-1, 0], residual x² + 2x + 1.
    let x1 = (t - V::ONE) * inv_dt;
    let before = x1 * x1 + x1 + x1 + V::ONE;
    let is_after = t.simd_lt(dt);
    let is_before = t.simd_gt(V::ONE - dt);
    is_after.select(after, is_before.select(before, V::ZERO))
}

/// The two-sample polynomial band-limited ramp residual — PolyBLEP's
/// integral, for a discontinuity in *slope* rather than value (Esqueda,
/// Välimäki & Bilbao, 2016).
///
/// Normalized like [`poly_blep`], which corrects a step of 2: this corrects a
/// slope change of 2 per sample, so the caller scales it by half the slope
/// change per sample.
#[inline(always)]
pub(crate) fn poly_blamp(t: V, dt: V, inv_dt: V) -> V {
    let third = V::splat(1.0 / 3.0);
    let x0 = t * inv_dt - V::ONE;
    let after = -(x0 * x0 * x0) * third;
    let x1 = (t - V::ONE) * inv_dt + V::ONE;
    let before = x1 * x1 * x1 * third;
    let is_after = t.simd_lt(dt);
    let is_before = t.simd_gt(V::ONE - dt);
    is_after.select(after, is_before.select(before, V::ZERO))
}

/// Wrap a phase that is at most one cycle past `1.0` back into `0.0..1.0`.
///
/// Cheaper than `x - floor(x)` and exact for the only inputs it gets: a phase
/// in `0.0..1.0` plus an increment below one.
#[inline(always)]
pub(crate) fn wrap_once(p: V) -> V {
    let wrapped = p - V::ONE;
    p.simd_lt(V::ONE).select(p, wrapped)
}

/// Band-limited sawtooth, rising from −1 to +1 over the cycle.
#[inline(always)]
pub(crate) fn saw(p: V, dt: V, inv_dt: V) -> V {
    // The naive ramp falls by 2 at the wrap; subtracting the step residual
    // rounds that fall.
    (p + p - V::ONE) - poly_blep(p, dt, inv_dt)
}

/// Band-limited pulse, `+1` for the first `width` of the cycle and `-1` after.
#[inline(always)]
pub(crate) fn pulse(p: V, width: V, dt: V, inv_dt: V) -> V {
    let naive = p.simd_lt(width).select(V::ONE, -V::ONE);
    // Rising edge at 0, falling edge at `width`.
    naive + poly_blep(p, dt, inv_dt) - poly_blep(wrap_once(p - width + V::ONE), dt, inv_dt)
}

/// Band-limited triangle, starting at 0 and rising, so a phase reset lands on
/// a zero crossing rather than a peak.
///
/// The naive triangle has continuous value but its slope flips between ±4 per
/// cycle at each corner — a change of 8 per cycle, `8·dt` per sample — so the
/// correction is a ramp residual, PolyBLAMP, scaled by half that change
/// (see [`poly_blamp`]): `4·dt`.
#[inline(always)]
pub(crate) fn triangle(p: V, dt: V, inv_dt: V) -> V {
    let half = V::splat(0.5);
    // `q` is the phase measured from the trough, so the corners sit at q = 0
    // (trough, slope -4 → +4) and q = 0.5 (peak, +4 → -4).
    let q = wrap_once(p + V::splat(0.25));
    let naive = V::ONE - V::splat(4.0) * (q - half).abs();
    let scale = V::splat(4.0) * dt;
    naive + scale * (poly_blamp(q, dt, inv_dt) - poly_blamp(wrap_once(q + half), dt, inv_dt))
}

/// Advance eight xorshift32 generators and return white noise in `-1.0..1.0`.
///
/// The top 23 bits become the mantissa of a float in `1.0..2.0`, which maps
/// linearly onto the output range with no division and no bias toward zero.
#[inline(always)]
pub(crate) fn white(state: &mut u32x8) -> V {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *state = x;
    let bits = (x >> 9) | u32x8::splat(0x3F80_0000);
    let one_to_two: V = wide::bytemuck::cast(bits);
    one_to_two + one_to_two - V::splat(3.0)
}

/// Pink noise from white, by Paul Kellet's three-pole "economy" filter.
///
/// Within ±0.05 dB of a −3 dB/octave slope from about 10 Hz to Nyquist at
/// 44.1 kHz, and three multiply-adds per lane — the refined seven-pole version
/// buys accuracy below 10 Hz, where a synth voice has nothing to say.
/// `OUTPUT_GAIN` brings the RMS to about 0.2, near a sine at −14 dBFS, so the
/// noise oscillator sits at a level comparable to the pitched ones.
#[inline(always)]
pub(crate) fn pink(white: V, b: &mut [V; 3]) -> V {
    const OUTPUT_GAIN: f32 = 0.12;
    b[0] = V::splat(0.997_65).mul_add(b[0], white * V::splat(0.099_046));
    b[1] = V::splat(0.963).mul_add(b[1], white * V::splat(0.296_516_4));
    b[2] = V::splat(0.57).mul_add(b[2], white * V::splat(1.052_691_3));
    (b[0] + b[1] + b[2] + white * V::splat(0.184_8)) * V::splat(OUTPUT_GAIN)
}

/// `tanh`, by the [7/6] Padé approximant clamped at its unity crossing.
///
/// The approximant reaches exactly `±1` near `|x| = 4.97` and is monotonic up
/// to there, so clamping the argument gives a saturator that never overshoots.
/// Worst error against `f64::tanh` is about `2e-4`, at the clamp; below
/// `|x| = 3` it is under `1e-6`. The ladder's feedback is the only caller, and
/// what it needs is a smooth, bounded, odd saturator — `wide`'s `tanh` is exact
/// to an ulp at roughly three times the cost, which the ladder pays per sample
/// per lane.
#[inline(always)]
pub(crate) fn tanh(x: V) -> V {
    let x = x.fast_clamp(V::splat(-4.97), V::splat(4.97));
    let x2 = x * x;
    let num = x * x2
        .mul_add(x2 + V::splat(378.0), V::splat(17_325.0))
        .mul_add(x2, V::splat(135_135.0));
    let den = V::splat(28.0)
        .mul_add(x2, V::splat(3_150.0))
        .mul_add(x2, V::splat(62_370.0))
        .mul_add(x2, V::splat(135_135.0));
    num / den
}

#[cfg(test)]
mod tests {
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
}
