use super::*;

// ── LfoShape waveform math (ported from tutti_nodes::lfo tests) ──

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
    assert!(shape(0.0, Depth::FULL, Polarity::Bipolar, CurveType::Linear).abs() < 1e-3);
    assert!((shape(1.0, Depth::FULL, Polarity::Bipolar, CurveType::Linear) - 1.0).abs() < 1e-2);
    assert!((shape(-1.0, Depth::FULL, Polarity::Bipolar, CurveType::Linear) + 1.0).abs() < 1e-2);
}

#[test]
fn shape_depth_scales_output() {
    assert!((shape(1.0, Depth(0.5), Polarity::Bipolar, CurveType::Linear) - 0.5).abs() < 1e-2);
}

/// A negative depth inverts the modulator rather than attenuating it — the
/// property that makes this a [`Depth`] and not an `Amplitude`.
#[test]
fn inverted_depth_flips_the_sign() {
    let up = shape(1.0, Depth::FULL, Polarity::Bipolar, CurveType::Linear);
    let down = shape(1.0, Depth::INVERTED, Polarity::Bipolar, CurveType::Linear);
    assert!((up + down).abs() < 1e-6, "{up} and {down} must cancel");
}

#[test]
fn shape_unipolar_maps_negative_to_zero_region() {
    // Unipolar remaps [-1,1]→[0,1]: x=-1 → 0, x=0 → 0.5, x=1 → 1.
    assert!(shape(-1.0, Depth::FULL, Polarity::Unipolar, CurveType::Linear).abs() < 1e-2);
    assert!((shape(0.0, Depth::FULL, Polarity::Unipolar, CurveType::Linear) - 0.5).abs() < 1e-2);
    assert!((shape(1.0, Depth::FULL, Polarity::Unipolar, CurveType::Linear) - 1.0).abs() < 1e-2);
}

/// The response curve `shape` applies to a magnitude `x ∈ [0, 1]`.
fn curve(c: CurveType, x: f32) -> f32 {
    shape(x, Depth::FULL, Polarity::Bipolar, c)
}

/// Every curve runs from 0 at no input to 1 at full input, `Bezier`
/// included.
///
/// Mutations (run), each fails: `shape`'s bipolar arm curving `1 - |x|`
/// → `Linear` at 0 is 1; the pin back on `audio-automation` `be7ccb9`,
/// whose `cubic_bezier` took its first point at 1 → `Bezier` at 0 is 1.
#[test]
fn every_curve_runs_from_zero_to_one() {
    for c in every_curve() {
        assert!(curve(c, 0.0).abs() < 1e-6, "{c:?} at 0: {}", curve(c, 0.0));
        assert!(
            (curve(c, 1.0) - 1.0).abs() < 1e-6,
            "{c:?} at 1: {}",
            curve(c, 1.0)
        );
    }
}

/// A bipolar `Bezier` route gives 0 at no input and is continuous
/// there: `-0.0`, `+0.0` and a hair either side all read 0, not a
/// full-depth jump from `-1` to `+1`.
///
/// Mutation (run): the pin back on `audio-automation` `be7ccb9` (its
/// `cubic_bezier` started at 1) → `+0.0` reads 1 and `-0.0` reads −1.
#[test]
fn a_bipolar_bezier_is_continuous_through_zero() {
    let c = CurveType::Bezier(0.25, 0.75);
    for x in [-1e-6_f32, -0.0, 0.0, 1e-6] {
        let v = shape(x, Depth::FULL, Polarity::Bipolar, c);
        assert!(v.abs() < 1e-5, "Bezier at {x:e}: {v}");
    }
    let (below, above) = (
        shape(-0.0, Depth::FULL, Polarity::Bipolar, c),
        shape(0.0, Depth::FULL, Polarity::Bipolar, c),
    );
    assert!((above - below).abs() < 1e-6, "jump at 0: {below} → {above}");
}

/// Each curve's value inside the range, from its closed form: `t^k` for
/// an in, `1 - (1 - t)^k` for an out, `2^(k-1)·t^k` on an in-out's
/// first half (the quadratic `SCurve` among them), `√t`,
/// `1 - √(1 - t²)`, `t²((s + 1)t - s)` with `s = 1.70158`, a step only
/// at the end, the Bezier `3(1-t)²t·c1 + 3(1-t)t²·c2 + t³` (midpoint
/// `(3·c1 + 3·c2 + 1)/8`), Elastic's `-2^(10(t-1))·sin((t - 1.075)·2π/0.3)`
/// (at 0.9: `-½ · sin(-7π/6) = -¼`), and Bounce's `1 - b(1 - t)` (at a
/// half, `b(½) = 7.5625·(1/22)² + 0.75`, so `15/64`).
///
/// Mutations (run), each fails: the smoothstep `SCurve`
/// (`x²(3 - 2x)`) → 0.15625 at a quarter, not 0.125; the easing family
/// read as linear (`_ => x`) → `QuadIn` at a half is 0.5, not 0.25;
/// `Stepped` linear → 0.5 at a half, not 0; the pin back on
/// `be7ccb9` → `Bezier(0, 1)` at a half is 0.625, not 0.5.
#[test]
fn each_curve_matches_its_closed_form() {
    let s = 1.701_58_f32;
    let cases: [(CurveType, f32, f32); 29] = [
        (CurveType::Linear, 0.5, 0.5),
        (CurveType::Exponential, 0.5, 0.25),
        (CurveType::Logarithmic, 0.25, 0.5),
        (
            CurveType::Logarithmic,
            0.5,
            core::f32::consts::FRAC_1_SQRT_2,
        ),
        (CurveType::SCurve, 0.25, 0.125),
        (CurveType::SCurve, 0.75, 0.875),
        (CurveType::Stepped, 0.5, 0.0),
        (CurveType::Stepped, 0.999, 0.0),
        (CurveType::Circular, 0.5, 1.0 - 0.75_f32.sqrt()),
        (CurveType::Back, 0.5, 0.25 * ((s + 1.0) * 0.5 - s)),
        (CurveType::Elastic, 0.9, -0.25),
        (CurveType::Bounce, 0.5, 15.0 / 64.0),
        (CurveType::QuadIn, 0.5, 0.25),
        (CurveType::QuadOut, 0.5, 0.75),
        (CurveType::QuadInOut, 0.25, 0.125),
        (CurveType::QuadInOut, 0.75, 0.875),
        (CurveType::CubicIn, 0.5, 0.125),
        (CurveType::CubicOut, 0.5, 0.875),
        (CurveType::CubicInOut, 0.25, 0.0625),
        (CurveType::CubicInOut, 0.75, 0.9375),
        (CurveType::QuartIn, 0.5, 0.0625),
        (CurveType::QuartOut, 0.5, 0.9375),
        (CurveType::QuartInOut, 0.25, 0.03125),
        (CurveType::QuintIn, 0.5, 0.03125),
        (CurveType::QuintOut, 0.5, 0.96875),
        (CurveType::QuintInOut, 0.25, 0.015625),
        // Control values 0 and 1 at the midpoint: (0 + 3 + 1)/8.
        (CurveType::Bezier(0.0, 1.0), 0.5, 0.5),
        (CurveType::Bezier(0.0, 1.0), 0.0, 0.0),
        (CurveType::Bezier(0.0, 1.0), 1.0, 1.0),
    ];
    for (c, x, want) in cases {
        let got = curve(c, x);
        assert!(
            (got - want).abs() < 1e-6,
            "{c:?} at {x}: {got}, want {want}"
        );
    }
}

/// `Back` and `Elastic` dip below 0 (minima about −0.10 and −0.373,
/// the figures the docs quote), and no curve exceeds 1.
///
/// Mutation (run): `shape`'s bipolar arm clamping the magnitude to
/// `[0, 1]` → `Back`'s minimum is 0.
#[test]
fn back_and_elastic_dip_below_zero_and_nothing_exceeds_one() {
    let min = |c| {
        (0..=100_000)
            .map(|i| curve(c, i as f32 / 100_000.0))
            .fold(f32::INFINITY, f32::min)
    };
    assert!((min(CurveType::Back) + 0.100_004).abs() < 1e-4);
    assert!((min(CurveType::Elastic) + 0.373_098).abs() < 1e-4);
    for c in every_curve() {
        for i in 0..=4096 {
            let v = curve(c, i as f32 / 4096.0);
            assert!(v <= 1.0 + 1e-6, "{c:?} at {}: {v}", i as f32 / 4096.0);
        }
    }
}

/// The monotone curves never fall as the input rises: every one but
/// `Back` and `Elastic` (which dip below 0 first) and `Bounce` (which
/// stays in `[0, 1]` but bounces).
///
/// Mutations (run), each fails: `shape` evaluating `QuadOut` as
/// `Bounce` → `QuadOut` falls; `shape` curving `1 - |x|` → every curve
/// falls.
#[test]
fn the_monotone_curves_never_fall() {
    let non_monotone = [CurveType::Elastic, CurveType::Bounce, CurveType::Back];
    for c in every_curve()
        .into_iter()
        .filter(|c| !non_monotone.contains(c))
    {
        let mut last = curve(c, 0.0);
        for i in 1..=1024 {
            let v = curve(c, i as f32 / 1024.0);
            assert!(
                v >= last,
                "{c:?} falls at {}: {last} → {v}",
                i as f32 / 1024.0
            );
            last = v;
        }
    }
}

/// `Quad` and `Cubic` shape the offset: they are not linear, and a
/// bipolar amount keeps the sign while curving the magnitude.
///
/// Mutations (run), each fails: `shape` reading every easing variant as
/// linear (`_ => x`) → `QuadIn` at 0.5 is 0.5; the unipolar arm
/// uncurved → `CubicIn` at 0 is 0.5.
#[test]
fn quad_and_cubic_are_not_linear() {
    for (c, want) in [
        (CurveType::QuadIn, 0.25),
        (CurveType::QuadOut, 0.75),
        (CurveType::CubicIn, 0.125),
        (CurveType::CubicOut, 0.875),
    ] {
        let up = shape(0.5, Depth::FULL, Polarity::Bipolar, c);
        let down = shape(-0.5, Depth::FULL, Polarity::Bipolar, c);
        assert!((up - want).abs() < 1e-6, "{c:?}: {up}, want {want}");
        assert!((down + want).abs() < 1e-6, "{c:?}: {down}, want {}", -want);
        assert!((up - 0.5).abs() > 0.1, "{c:?} is linear");
    }
    // Unipolar: x = 0 is the middle of the curve.
    let mid = shape(0.0, Depth::FULL, Polarity::Unipolar, CurveType::CubicIn);
    assert!((mid - 0.125).abs() < 1e-6, "CubicIn unipolar at 0: {mid}");
}

#[test]
fn shape_scurve_is_flatter_at_extremes() {
    // SCurve magnitude at small |x| is below linear.
    let lin = shape(0.25, Depth::FULL, Polarity::Bipolar, CurveType::Linear);
    let scv = shape(0.25, Depth::FULL, Polarity::Bipolar, CurveType::SCurve);
    assert!(scv < lin);
}
