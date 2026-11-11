use super::*;

/// A step closes `coeff` of the gap: a tenth up, a tenth down, all of it
/// at 1, none at 0.
#[test]
fn a_step_closes_the_coefficient_of_the_gap() {
    assert!((Smoother::toward(0.0, 1.0, 0.1) - 0.1).abs() < 0.001);
    assert!((Smoother::toward(1.0, 0.0, 0.1) - 0.9).abs() < 0.001);
    assert_eq!(Smoother::toward(0.0, 1.0, 1.0), 1.0);
    assert_eq!(Smoother::toward(0.25, 1.0, 0.0), 0.25);
    let mut s = Smoother::new(0.5);
    s.seed(0.0);
    assert_eq!(s.step(1.0), 0.5);
    assert_eq!(s.step(1.0), 0.75);
    assert_eq!(s.value(), 0.75);
}

/// `advance` is `steps` single steps, within the rounding bound it
/// documents (`1.5 * 2^-24 * R * min(steps, 1/coeff)`, with a factor of
/// two for the closed form's own rounding), and exact for zero steps and
/// for an instant smoother.
#[test]
fn advance_is_the_closed_form_of_repeated_steps() {
    for coeff in [0.0004_f32, 0.02, 0.3] {
        for steps in [1usize, 7, 64, 1024, 48_000] {
            let mut looped = Smoother::new(coeff);
            looped.seed(-30.0);
            for _ in 0..steps {
                looped.step(90.0);
            }
            let mut closed = Smoother::new(coeff);
            closed.seed(-30.0);
            let got = closed.advance(90.0, steps);
            let range = 120.0_f64;
            let bound =
                2.0 * 1.5 * 2f64.powi(-24) * range * (steps as f64).min(1.0 / f64::from(coeff));
            let exact = 90.0 - 120.0 * (1.0 - f64::from(coeff)).powi(steps as i32);
            assert!(
                (f64::from(got) - exact).abs() <= bound + 1e-6,
                "{coeff} x {steps}: closed {got} vs exact {exact}"
            );
            assert!(
                (f64::from(looped.value()) - f64::from(got)).abs() <= 2.0 * bound + 1e-6,
                "{coeff} x {steps}: loop {} vs closed {got}",
                looped.value()
            );
        }
    }
    let mut s = Smoother::new(0.1);
    s.seed(10.0);
    assert_eq!(s.advance(70.0, 0), 10.0);
    let mut instant = Smoother::new(1.0);
    instant.seed(10.0);
    assert_eq!(instant.advance(70.0, 5), 70.0);
}

/// The fraction closed is `1 - (1 - c)^n`: nothing at zero steps, `c` at
/// one, all of it at an instant coefficient, and a time constant's worth
/// of steps (`1 / c`) closes 63%.
#[test]
fn the_gap_closed_is_one_minus_the_retention_to_the_steps() {
    let s = Smoother::new(0.01);
    assert_eq!(s.gap_closed(0), 0.0);
    assert!((s.gap_closed(1) - 0.01).abs() < 1e-9);
    assert!((s.gap_closed(100) - (1.0 - (-1.0_f32).exp())).abs() < 3e-3);
    assert_eq!(Smoother::new(1.0).gap_closed(3), 1.0);
}
