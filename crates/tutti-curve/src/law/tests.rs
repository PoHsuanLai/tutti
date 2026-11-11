//! Each law against its closed form. The spec is the oracle: the expected
//! figures are written out here, not read from another implementation.

use super::Law;
use crate::{Curvature, Exponent, Piece};

const LAWS: [Law; 9] = [
    Law::Linear,
    Law::Hold,
    Law::PowerIn(Exponent::of(2.0)),
    Law::PowerOut(Exponent::of(3.0)),
    Law::PowerInOut(Exponent::of(2.0)),
    Law::PowerIn(Exponent::of(1.7)),
    Law::Exp(Curvature::of(3.0)),
    Law::Exp(Curvature::of(-2.5)),
    Law::Smootherstep,
];

/// The unit steps: `Linear` is `t`, `Hold` is 0 until the end.
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
#[test]
fn integer_powers_are_exact() {
    for (p, p_in) in [
        (1.0, 0.5),
        (2.0, 0.25),
        (3.0, 0.125),
        (4.0, 0.0625),
        (5.0, 0.03125),
    ] {
        assert_eq!(
            Law::PowerIn(Exponent::of(p)).eval(0.5),
            p_in,
            "PowerIn({p}) at 1/2"
        );
        assert_eq!(
            Law::PowerOut(Exponent::of(p)).eval(0.5),
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
            Law::PowerInOut(Exponent::of(p)).eval(0.25),
            at_quarter,
            "PowerInOut({p}) at 1/4"
        );
        assert_eq!(
            Law::PowerInOut(Exponent::of(p)).eval(0.75),
            1.0 - at_quarter,
            "PowerInOut({p}) at 3/4"
        );
        assert_eq!(
            Law::PowerInOut(Exponent::of(p)).eval(0.5),
            0.5,
            "PowerInOut({p}) at 1/2"
        );
    }
}

/// An exponent of one half is the square root (exact: `sqrt(1/4) = 1/2`), and
/// any other exponent is `t^p` to `f32` accuracy.
#[test]
fn other_exponents_follow_the_power() {
    assert_eq!(Law::PowerIn(Exponent::of(0.5)).eval(0.25), 0.5);
    for (p, t) in [(1.7f32, 0.3f32), (0.3, 0.9), (7.5, 0.6)] {
        let want = f64::from(t).powf(f64::from(p));
        assert!(
            (f64::from(Law::PowerIn(Exponent::of(p)).eval(t)) - want).abs() < 1e-6,
            "PowerIn({p}) at {t}"
        );
        let want = 1.0 - (1.0 - f64::from(t)).powf(f64::from(p));
        assert!(
            (f64::from(Law::PowerOut(Exponent::of(p)).eval(t)) - want).abs() < 1e-6,
            "PowerOut({p}) at {t}"
        );
    }
}

/// `Exp(k)` is `(e^(kt) - 1) / (e^k - 1)`, so with `k = ln(to / from)` the
/// value is geometric: `from · (to / from)^t`.
#[test]
fn exp_is_the_exponential_and_geometric_at_log_ratio() {
    for k in [3.0f32, -2.5, 0.5, 20.0] {
        for t in [0.1f32, 0.5, 0.9] {
            let (k64, t64) = (f64::from(k), f64::from(t));
            let want = (k64 * t64).exp_m1() / k64.exp_m1();
            assert!(
                (f64::from(Law::Exp(Curvature::of(k)).eval(t)) - want).abs() < 1e-5,
                "Exp({k}) at {t}"
            );
        }
    }
    let (from, to) = (100.0f32, 1600.0f32);
    let k = (to / from).ln();
    for t in [0.25f32, 0.5, 0.75] {
        let want = f64::from(from) * (f64::from(to) / f64::from(from)).powf(f64::from(t));
        let got = f64::from(Piece::new(from, to, Law::Exp(Curvature::of(k))).value(t));
        assert!(
            (got - want).abs() < 1e-3 * want,
            "geometric at {t}: {got} vs {want}"
        );
    }
}

/// `Exp` near zero curvature is linear, not `0 / 0`.
#[test]
fn exp_at_zero_curvature_is_linear() {
    for k in [0.0, 1e-6, -1e-6] {
        assert_eq!(Law::Exp(Curvature::of(k)).eval(0.3), 0.3, "Exp({k})");
    }
}

/// The extreme curvatures a `Curvature` holds are evaluated as they are, not
/// clamped, and the ratio never reaches `inf / inf`.
#[test]
fn the_extreme_curvatures_stay_finite_and_monotone() {
    for k in [Curvature::MAX, -Curvature::MAX] {
        let law = Law::Exp(Curvature::of(k));
        let mut last = 0.0;
        for i in 0..=100 {
            let v = law.eval(i as f32 / 100.0);
            assert!(
                v.is_finite() && v >= last && v <= 1.0,
                "Exp({k}) at {i}: {v}"
            );
            last = v;
        }
        assert_eq!(law.eval(1.0), 1.0);
    }
}

/// Progress is clamped to `[0, 1]` and a NaN progress is 0.
#[test]
fn progress_is_clamped() {
    for law in LAWS {
        assert_eq!(law.eval(-0.5), law.eval(0.0), "{law:?} below");
        assert_eq!(law.eval(2.0), law.eval(1.0), "{law:?} above");
        assert_eq!(law.eval(f32::NAN), 0.0, "{law:?} at NaN");
    }
}

// Moved from `tutti-graph`'s `event.rs` tests, unchanged but for the name.
/// Every law runs from 0 to 1, monotonically, at its analytic figures
/// (closed forms in `f64`, within a tolerance: `Exp` and a non-integer
/// power go through libm).
#[test]
fn every_law_meets_its_closed_form() {
    use Law as S;
    let closed = |s: S, t: f64| -> f64 {
        match s {
            S::Linear => t,
            S::Hold => f64::from(u8::from(t >= 1.0)),
            S::PowerIn(p) => t.powf(f64::from(p.get())),
            S::PowerOut(p) => 1.0 - (1.0 - t).powf(f64::from(p.get())),
            S::PowerInOut(p) if t < 0.5 => {
                2f64.powf(f64::from(p.get()) - 1.0) * t.powf(f64::from(p.get()))
            }
            S::PowerInOut(p) => 1.0 - (2.0 - 2.0 * t).powf(f64::from(p.get())) / 2.0,
            S::Exp(k) => {
                let k = f64::from(k.get());
                ((k * t).exp() - 1.0) / (k.exp() - 1.0)
            }
            S::Smootherstep => t * t * t * (6.0 * t * t - 15.0 * t + 10.0),
        }
    };
    let shapes = [
        S::Linear,
        S::Hold,
        S::PowerIn(Exponent::of(2.0)),
        S::PowerIn(Exponent::of(0.5)),
        S::PowerIn(Exponent::of(5.0)),
        S::PowerIn(Exponent::of(1.7)),
        S::PowerOut(Exponent::of(3.0)),
        S::PowerInOut(Exponent::of(2.0)),
        S::PowerInOut(Exponent::of(4.0)),
        S::Exp(Curvature::of(5.0)),
        S::Smootherstep,
        S::Exp(Curvature::of(-3.0)),
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
            // `Smootherstep`'s `f32` Horner form loses up to 2.3e-6 of
            // monotonicity to cancellation near 1 (see its docs).
            let slack = if s == S::Smootherstep { 3e-6 } else { 0.0 };
            assert!(g + slack >= prev, "{s:?} is monotone at {t}");
            prev = g;
        }
    }
    // Out of range and NaN progress: clamped, never a NaN.
    assert_eq!(S::Exp(Curvature::of(2.0)).eval(-1.0), 0.0);
    assert_eq!(S::PowerIn(Exponent::of(2.0)).eval(f32::NAN), 0.0);
}

/// `Linear` returns `t` bit for bit on `[0, 1]` (the clamp is the identity
/// there), so a linear [`Piece`] is `from + (to - from) * t` to the bit: what
/// the graph's edge-gain ramp and param ramps write.
#[test]
fn linear_is_the_identity_on_the_unit_interval() {
    for n in [1u32, 3, 7, 64, 97, 128, 1000] {
        for i in 0..n {
            let t = (i + 1) as f32 / n as f32;
            assert_eq!(Law::Linear.eval(t).to_bits(), t.to_bits());
            let (from, to) = (0.3f32, -7.9f32);
            assert_eq!(
                Piece::new(from, to, Law::Linear).value(t).to_bits(),
                (from + (to - from) * t).to_bits()
            );
        }
    }
}
