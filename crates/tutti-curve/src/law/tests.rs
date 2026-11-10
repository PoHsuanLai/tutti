//! Each law against its closed form. The spec is the oracle: the expected
//! figures are written out here, not read from another implementation.

use super::Law;

const LAWS: [Law; 8] = [
    Law::Linear,
    Law::Hold,
    Law::PowerIn(2.0),
    Law::PowerOut(3.0),
    Law::PowerInOut(2.0),
    Law::PowerIn(1.7),
    Law::Exp(3.0),
    Law::Exp(-2.5),
];

/// The unit steps: `Linear` is `t`, `Hold` is 0 until the end.
///
/// Mutation (run): `Hold` as `t < 0.5` fails at 0.75 and 0.99.
#[test]
fn linear_is_t_and_hold_steps_at_the_end() {
    for t in [0.0, 0.125, 0.5, 0.75, 1.0] {
        assert_eq!(Law::Linear.eval(t), t);
    }
    for t in [0.0, 0.25, 0.75, 0.999_999] {
        assert_eq!(Law::Hold.eval(t), 0.0, "Hold at {t}");
    }
    assert_eq!(Law::Hold.eval(1.0), 1.0);
}

/// The integer exponents are multiplications: exact in binary fractions.
/// `t^p` at 1/2 is `2^-p`; `1 - (1 - t)^p` at 1/2 is `1 - 2^-p`; the
/// in-out law is `2^(p-1) t^p` below 1/2, so at 1/4 it is `2^(p-1) 4^-p`.
///
/// Mutation (run): `pow` taking the `p == 3.0` arm as `x * x` fails
/// `PowerIn(3)`; `PowerInOut`'s `/ 2.0` dropped fails every in-out row;
/// `PowerOut` as `pow(1.0 - t, p)` (no outer `1 -`) fails every out row.
#[test]
fn integer_powers_are_exact() {
    for (p, p_in) in [
        (1.0, 0.5),
        (2.0, 0.25),
        (3.0, 0.125),
        (4.0, 0.0625),
        (5.0, 0.03125),
    ] {
        assert_eq!(Law::PowerIn(p).eval(0.5), p_in, "PowerIn({p}) at 1/2");
        assert_eq!(
            Law::PowerOut(p).eval(0.5),
            1.0 - p_in,
            "PowerOut({p}) at 1/2"
        );
    }
    for (p, at_quarter) in [
        (2.0, 0.125),
        (3.0, 0.0625),
        (4.0, 0.03125),
        (5.0, 0.015_625),
    ] {
        assert_eq!(
            Law::PowerInOut(p).eval(0.25),
            at_quarter,
            "PowerInOut({p}) at 1/4"
        );
        assert_eq!(
            Law::PowerInOut(p).eval(0.75),
            1.0 - at_quarter,
            "PowerInOut({p}) at 3/4"
        );
        assert_eq!(Law::PowerInOut(p).eval(0.5), 0.5, "PowerInOut({p}) at 1/2");
    }
}

/// An exponent of one half is the square root (exact: `sqrt(1/4) = 1/2`), and
/// any other exponent is `t^p` to `f32` accuracy.
///
/// Mutations (run), each fails: the `p == 0.5` arm returning `x` fails the
/// exact figure; the `powf` arm as `x * p` fails the loose one.
#[test]
fn other_exponents_follow_the_power() {
    assert_eq!(Law::PowerIn(0.5).eval(0.25), 0.5);
    for (p, t) in [(1.7f32, 0.3f32), (0.3, 0.9), (7.5, 0.6)] {
        let want = f64::from(t).powf(f64::from(p));
        assert!(
            (f64::from(Law::PowerIn(p).eval(t)) - want).abs() < 1e-6,
            "PowerIn({p}) at {t}"
        );
        let want = 1.0 - (1.0 - f64::from(t)).powf(f64::from(p));
        assert!(
            (f64::from(Law::PowerOut(p).eval(t)) - want).abs() < 1e-6,
            "PowerOut({p}) at {t}"
        );
    }
}

/// `Exp(k)` is `(e^(kt) - 1) / (e^k - 1)`, so with `k = ln(to / from)` the
/// value is geometric: `from · (to / from)^t`.
///
/// Mutation (run): `k.exp_m1()` in the denominator replaced by `k.exp()` fails
/// both checks; the `k` clamp removed fails `a_huge_curvature_is_clamped`.
#[test]
fn exp_is_the_exponential_and_geometric_at_log_ratio() {
    for k in [3.0f32, -2.5, 0.5, 20.0] {
        for t in [0.1f32, 0.5, 0.9] {
            let (k64, t64) = (f64::from(k), f64::from(t));
            let want = (k64 * t64).exp_m1() / k64.exp_m1();
            assert!(
                (f64::from(Law::Exp(k).eval(t)) - want).abs() < 1e-5,
                "Exp({k}) at {t}"
            );
        }
    }
    let (from, to) = (100.0f32, 1600.0f32);
    let k = (to / from).ln();
    for t in [0.25f32, 0.5, 0.75] {
        let want = f64::from(from) * (f64::from(to) / f64::from(from)).powf(f64::from(t));
        let got = f64::from(Law::Exp(k).interpolate(from, to, t));
        assert!(
            (got - want).abs() < 1e-3 * want,
            "geometric at {t}: {got} vs {want}"
        );
    }
}

/// `Exp` near zero curvature is linear, not `0 / 0`.
///
/// Mutation (run): the `k.abs() < 1e-4` branch removed gives NaN at `k = 0`.
#[test]
fn exp_at_zero_curvature_is_linear() {
    for k in [0.0, 1e-6, -1e-6] {
        assert_eq!(Law::Exp(k).eval(0.3), 0.3, "Exp({k})");
    }
}

/// `|k|` past 64 is clamped to 64, so the ratio never reaches `inf / inf`.
///
/// Mutation (run): the clamp removed gives NaN for `Exp(1000.0)`.
#[test]
fn a_huge_curvature_is_clamped() {
    assert_eq!(Law::Exp(1000.0).eval(0.5), Law::Exp(64.0).eval(0.5));
    assert_eq!(Law::Exp(-1000.0).eval(0.5), Law::Exp(-64.0).eval(0.5));
    assert!(Law::Exp(1000.0).eval(0.5).is_finite());
}

/// A parameter out of range or not finite is a linear law, never a NaN.
///
/// Mutation (run): `valid_exponent` returning `true` fails every power row
/// (`powf` of a NaN, of a negative exponent's reciprocal); the `is_finite`
/// guard on `Exp` removed fails the NaN and infinity rows.
#[test]
fn an_invalid_parameter_is_linear() {
    let bad = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY];
    for p in bad.into_iter().chain([0.0, -1.0]) {
        for law in [Law::PowerIn(p), Law::PowerOut(p), Law::PowerInOut(p)] {
            assert_eq!(law.eval(0.3), 0.3, "{law:?}");
        }
    }
    for k in bad {
        assert_eq!(Law::Exp(k).eval(0.3), 0.3, "Exp({k})");
    }
}

/// Progress is clamped to `[0, 1]` and a NaN progress is 0.
///
/// Mutation (run): the clamp's `.min(1.0)` removed gives `Linear.eval(2.0)` 2;
/// `t >= 0.0` written `t > -1.0` gives `Linear.eval(-0.5)` -0.5.
#[test]
fn progress_is_clamped() {
    for law in LAWS {
        assert_eq!(law.eval(-0.5), law.eval(0.0), "{law:?} below");
        assert_eq!(law.eval(2.0), law.eval(1.0), "{law:?} above");
        assert_eq!(law.eval(f32::NAN), 0.0, "{law:?} at NaN");
    }
}

/// `interpolate` is `from + (to - from) · g(t)`, in either direction.
///
/// Mutation (run): `from + (to - from)` written `to + (from - to)` fails the
/// endpoints; the factor `g` dropped (`from + (to - from) * t`) fails the
/// quadratic.
#[test]
fn interpolate_scales_the_law_onto_the_endpoints() {
    assert_eq!(Law::Linear.interpolate(2.0, 6.0, 0.25), 3.0);
    assert_eq!(Law::PowerIn(2.0).interpolate(2.0, 6.0, 0.5), 3.0);
    assert_eq!(Law::PowerIn(2.0).interpolate(6.0, 2.0, 0.5), 5.0);
    assert_eq!(Law::Hold.interpolate(2.0, 6.0, 0.5), 2.0);
    assert_eq!(Law::Hold.interpolate(2.0, 6.0, 1.0), 6.0);
}

// Moved from `tutti-graph`'s `event.rs` tests, unchanged but for the name.
/// Every law runs from 0 to 1, monotonically, at its analytic figures
/// (closed forms in `f64`, within a tolerance: `Exp` and a non-integer
/// power go through libm), and a law with an unusable parameter is
/// linear.
///
/// Mutations (run), each fails: `PowerOut` as `pow(t, p)` (the ease-in);
/// `PowerInOut`'s first half as `pow(t, p)` (no `2t`); `Exp` with
/// `exp_m1(k)` and `exp_m1(k t)` swapped; dropping the `valid_exponent`
/// guard (`PowerIn(-1)` gives `inf` at 0).
#[test]
fn every_law_meets_its_closed_form() {
    use Law as S;
    let closed = |s: S, t: f64| -> f64 {
        match s {
            S::Linear => t,
            S::Hold => f64::from(u8::from(t >= 1.0)),
            S::PowerIn(p) => t.powf(f64::from(p)),
            S::PowerOut(p) => 1.0 - (1.0 - t).powf(f64::from(p)),
            S::PowerInOut(p) if t < 0.5 => 2f64.powf(f64::from(p) - 1.0) * t.powf(f64::from(p)),
            S::PowerInOut(p) => 1.0 - (2.0 - 2.0 * t).powf(f64::from(p)) / 2.0,
            S::Exp(k) => {
                let k = f64::from(k);
                ((k * t).exp() - 1.0) / (k.exp() - 1.0)
            }
        }
    };
    let shapes = [
        S::Linear,
        S::Hold,
        S::PowerIn(2.0),
        S::PowerIn(0.5),
        S::PowerIn(5.0),
        S::PowerIn(1.7),
        S::PowerOut(3.0),
        S::PowerInOut(2.0),
        S::PowerInOut(4.0),
        S::Exp(5.0),
        S::Exp(-3.0),
    ];
    for s in shapes {
        assert_eq!(s.eval(0.0), 0.0, "{s:?} starts at 0");
        assert_eq!(s.eval(1.0), 1.0, "{s:?} ends at 1");
        let mut prev = 0.0f32;
        for i in 0..=1000 {
            let t = i as f32 / 1000.0;
            let g = s.eval(t);
            let want = closed(s, f64::from(t));
            assert!(
                (f64::from(g) - want).abs() < 2e-6,
                "{s:?} at {t}: {g} vs {want}"
            );
            assert!(g >= prev, "{s:?} is monotone at {t}");
            prev = g;
        }
    }
    for s in [
        S::PowerIn(-1.0),
        S::PowerOut(0.0),
        S::PowerInOut(f32::INFINITY),
        S::Exp(f32::NAN),
    ] {
        assert_eq!(s.eval(0.0), 0.0, "{s:?}");
        assert_eq!(s.eval(0.3), 0.3, "{s:?} is linear");
    }
    // Out of range and NaN progress: clamped, never a NaN.
    assert_eq!(S::Exp(2.0).eval(-1.0), 0.0);
    assert_eq!(S::PowerIn(2.0).eval(f32::NAN), 0.0);
    assert!(S::Exp(1e9).eval(0.5).is_finite());
}
