use super::*;

/// The curves, pinned.
///
/// The formulas follow the common waveshaper definitions; these values
/// pin them.
///
/// The four curves built from `+ * / round floor clamp abs` are pinned
/// **exactly**: IEEE-754 rounds each of those correctly, so the result is
/// the same on every platform. `tanh` and `atan` are libm
/// quality-of-implementation and differ in the last ulp between C runtimes
/// (the same reason `tutti-nodes`'s click digest is gated off MSVC), so those
/// two are pinned against the f64 closed form with a tolerance instead.
#[test]
fn curves_are_pinned() {
    let exact = [
        (ShapeKind::HardClip, 0.75, 2.0, 1.0),
        (ShapeKind::HardClip, -0.2, 2.0, -0.4),
        (ShapeKind::HardClip, -3.0, 1.0, -1.0),
        (ShapeKind::Softsign, 1.0, 1.0, 0.5),
        (ShapeKind::Softsign, -3.0, 1.0, -0.75),
        (ShapeKind::Crush, 0.3, 4.0, 0.25),
        // Drive below one level per unit floors to one.
        (ShapeKind::Crush, 0.6, 0.5, 1.0),
        (ShapeKind::Crush, 0.3, 0.5, 0.0),
        // SoftCrush passes through each step's midpoint and each integer.
        (ShapeKind::SoftCrush, 1.5, 1.0, 1.5),
        (ShapeKind::SoftCrush, 1.0, 1.0, 1.0),
        (ShapeKind::SoftCrush, 0.25, 2.0, 0.25),
    ];
    for (kind, x, drive, want) in exact {
        let got = kind.apply(x, Drive(drive));
        assert_eq!(
            got.to_bits(),
            f32::to_bits(want),
            "{kind:?}({x}, drive {drive}) = {got}, want exactly {want}"
        );
    }

    // Off the midpoint the S-curve is a rational: smooth9(1/4) is
    // 12826 / 262144 exactly, so at hardness 1 a quarter step lands there.
    let quarter = ShapeKind::SoftCrush.apply(0.25, Drive(1.0));
    assert!((f64::from(quarter) - 12826.0 / 262144.0).abs() < 1e-7);

    let half_pi = std::f64::consts::FRAC_PI_2;
    let closed = [
        (ShapeKind::Tanh, 0.5f32, 2.0f32, 1.0f64.tanh()),
        (ShapeKind::Tanh, -1.0, 3.0, (-3.0f64).tanh()),
        (ShapeKind::Atan, 1.0, 1.0, half_pi.atan() / half_pi),
        (
            ShapeKind::Atan,
            -0.5,
            4.0,
            (-half_pi * 2.0).atan() / half_pi,
        ),
    ];
    for (kind, x, drive, want) in closed {
        let got = f64::from(kind.apply(x, Drive(drive)));
        assert!(
            (got - want).abs() < 1e-6,
            "{kind:?}({x}, drive {drive}) = {got}, want {want}"
        );
    }
}
