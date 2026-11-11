use super::*;

// ── LfoShape waveform math ──

#[test]
fn test_lfo_shapes() {
    let sine_val = LfoShape::Sine.evaluate_periodic(Phase(0.25));
    assert!((sine_val - 1.0).abs() < 0.01);

    let square_val = LfoShape::Square.evaluate_periodic(Phase(0.25));
    assert_eq!(square_val, 1.0);

    let square_val2 = LfoShape::Square.evaluate_periodic(Phase(0.75));
    assert_eq!(square_val2, -1.0);

    let tri_val = LfoShape::Triangle.evaluate_periodic(Phase(0.25));
    assert!((tri_val - 1.0).abs() < 0.01);

    let saw_val = LfoShape::Sawtooth.evaluate_periodic(Phase(0.5));
    assert!((saw_val - 0.0).abs() < 0.01);
}

#[test]
fn test_sawtooth_down_shape() {
    let val_start = LfoShape::SawtoothDown.evaluate_periodic(Phase(0.0));
    assert!((val_start - 1.0).abs() < 0.01);

    let val_mid = LfoShape::SawtoothDown.evaluate_periodic(Phase(0.5));
    assert!((val_mid - 0.0).abs() < 0.01);

    // Phase 1.0 is the cycle's end — reachable only as a literal, since
    // `wrapped` would fold it to the next cycle's start.
    let val_end = LfoShape::SawtoothDown.evaluate_periodic(Phase(1.0));
    assert!((val_end - (-1.0)).abs() < 0.01);
}

// ── shape() ──

#[test]
fn shape_linear_bipolar_is_depth_scaled_identity() {
    assert!(shape(0.0, Depth::FULL, Polarity::Bipolar, Law::Linear).abs() < 1e-3);
    assert!((shape(1.0, Depth::FULL, Polarity::Bipolar, Law::Linear) - 1.0).abs() < 1e-2);
    assert!((shape(-1.0, Depth::FULL, Polarity::Bipolar, Law::Linear) + 1.0).abs() < 1e-2);
}

#[test]
fn shape_depth_scales_output() {
    assert!((shape(1.0, Depth(0.5), Polarity::Bipolar, Law::Linear) - 0.5).abs() < 1e-2);
}

/// A negative depth inverts the modulator rather than attenuating it — the
/// property that makes this a [`Depth`] and not an `Amplitude`.
#[test]
fn inverted_depth_flips_the_sign() {
    let up = shape(1.0, Depth::FULL, Polarity::Bipolar, Law::Linear);
    let down = shape(1.0, Depth::INVERTED, Polarity::Bipolar, Law::Linear);
    assert!((up + down).abs() < 1e-6, "{up} and {down} must cancel");
}

#[test]
fn shape_unipolar_maps_negative_to_zero_region() {
    // Unipolar remaps [-1,1]→[0,1]: x=-1 → 0, x=0 → 0.5, x=1 → 1.
    assert!(shape(-1.0, Depth::FULL, Polarity::Unipolar, Law::Linear).abs() < 1e-2);
    assert!((shape(0.0, Depth::FULL, Polarity::Unipolar, Law::Linear) - 0.5).abs() < 1e-2);
    assert!((shape(1.0, Depth::FULL, Polarity::Unipolar, Law::Linear) - 1.0).abs() < 1e-2);
}

/// The laws these tests shape through: one of each kind, with the power laws
/// at an integer and a fractional exponent. The laws' own figures are
/// `tutti-curve`'s tests.
const LAWS: [Law; 8] = [
    Law::Linear,
    Law::Hold,
    Law::PowerIn(2.0),
    Law::PowerOut(3.0),
    Law::PowerInOut(2.0),
    Law::PowerIn(0.5),
    Law::PowerIn(1.7),
    Law::Exp(3.0),
];

/// The response curve `shape` applies to a magnitude `x ∈ [0, 1]`.
fn curve(c: Law, x: f32) -> f32 {
    shape(x, Depth::FULL, Polarity::Bipolar, c)
}

/// Every curve runs from 0 at no input to 1 at full input.
#[test]
fn every_curve_runs_from_zero_to_one() {
    for c in LAWS {
        assert!(curve(c, 0.0).abs() < 1e-6, "{c:?} at 0: {}", curve(c, 0.0));
        assert!(
            (curve(c, 1.0) - 1.0).abs() < 1e-6,
            "{c:?} at 1: {}",
            curve(c, 1.0)
        );
    }
}

/// No curve falls as the input rises, and none leaves `[0, 1]`: the offset
/// never exceeds the depth.
#[test]
fn no_curve_falls_or_leaves_the_unit_range() {
    for c in LAWS {
        let mut last = curve(c, 0.0);
        for i in 1..=1024 {
            let v = curve(c, i as f32 / 1024.0);
            assert!(
                v >= last,
                "{c:?} falls at {}: {last} → {v}",
                i as f32 / 1024.0
            );
            assert!(v <= 1.0, "{c:?} exceeds 1 at {}: {v}", i as f32 / 1024.0);
            last = v;
        }
    }
}

/// `Quad` and `Cubic` shape the offset: they are not linear, and a
/// bipolar amount keeps the sign while curving the magnitude.
#[test]
fn quad_and_cubic_are_not_linear() {
    for (c, want) in [
        (Law::PowerIn(2.0), 0.25),
        (Law::PowerOut(2.0), 0.75),
        (Law::PowerIn(3.0), 0.125),
        (Law::PowerOut(3.0), 0.875),
    ] {
        let up = shape(0.5, Depth::FULL, Polarity::Bipolar, c);
        let down = shape(-0.5, Depth::FULL, Polarity::Bipolar, c);
        assert!((up - want).abs() < 1e-6, "{c:?}: {up}, want {want}");
        assert!((down + want).abs() < 1e-6, "{c:?}: {down}, want {}", -want);
        assert!((up - 0.5).abs() > 0.1, "{c:?} is linear");
    }
    // Unipolar: x = 0 is the middle of the curve.
    let mid = shape(0.0, Depth::FULL, Polarity::Unipolar, Law::PowerIn(3.0));
    assert!((mid - 0.125).abs() < 1e-6, "CubicIn unipolar at 0: {mid}");
}

#[test]
fn shape_scurve_is_flatter_at_extremes() {
    // SCurve magnitude at small |x| is below linear.
    let lin = shape(0.25, Depth::FULL, Polarity::Bipolar, Law::Linear);
    let scv = shape(0.25, Depth::FULL, Polarity::Bipolar, Law::PowerInOut(2.0));
    assert!(scv < lin);
}
