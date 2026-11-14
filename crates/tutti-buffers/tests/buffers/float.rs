//! `Float` is zero, one and the analytic values of its math.

use tutti_buffers::Float;

fn laws<F: Float>(tol: F) {
    let near = |a: F, b: F| (a - b).abs() <= tol;
    assert_eq!(F::ZERO + F::ONE, F::ONE);
    let two = F::ONE + F::ONE;
    assert_eq!((two * two).sqrt(), two);
    assert!(near(F::ZERO.exp(), F::ONE));
    assert!(near(F::ONE.ln(), F::ZERO));
    assert!(near(two.powf(two), two * two));
    assert!(near(F::ZERO.sin(), F::ZERO));
    assert!(near(F::ZERO.cos(), F::ONE));
    assert_eq!((-two).abs(), two);
    let half = F::ONE / two;
    assert_eq!((F::ONE + half).floor(), F::ONE);
    assert_eq!((F::ONE + half).ceil(), two);
    assert_eq!((-half).floor(), -F::ONE);
    assert!(near(F::ZERO.tan(), F::ZERO));
    assert!(near(F::ZERO.tanh(), F::ZERO));
    assert_eq!(two.min(half), half);
    assert_eq!(two.max(half), two);
}

/// tan(π/4) = 1, tanh saturates at ±1, and min/max return the number when the
/// other side is NaN (IEEE 754 minNum/maxNum).
#[test]
fn tan_tanh_min_max_meet_their_analytic_values() {
    assert!((<f64 as Float>::tan(core::f64::consts::FRAC_PI_4) - 1.0).abs() < 1e-15);
    assert!((<f32 as Float>::tan(core::f32::consts::FRAC_PI_4) - 1.0).abs() < 1e-6);
    assert_eq!(<f64 as Float>::tanh(40.0), 1.0);
    assert_eq!(<f32 as Float>::tanh(-40.0), -1.0);
    assert_eq!(<f32 as Float>::min(f32::NAN, 0.5), 0.5);
    assert_eq!(<f64 as Float>::max(-0.5, f64::NAN), -0.5);
}

#[test]
fn f32_math_matches_its_analytic_values() {
    laws::<f32>(1e-6);
}

#[test]
fn f64_math_matches_its_analytic_values() {
    laws::<f64>(1e-12);
}

#[test]
fn exp_of_one_is_euler_s_number() {
    assert!((<f64 as Float>::exp(1.0) - core::f64::consts::E).abs() < 1e-15);
    assert!((<f32 as Float>::exp(1.0) - core::f32::consts::E).abs() < 1e-6);
}
