use super::*;

/// What is deliberately *absent* is as load-bearing as what is present.
/// These must not compile; the ledger is here so a future contributor does
/// not "helpfully" add them.
///
/// - `Mix + Mix`, `Mix * f32` — a blend *position* does
///   not compose or scale; half of a crossfade point is not a crossfade
///   point. (`Depth` has a sign, and its own range check — that is the difference
///   between a position and a signed magnitude.)
/// - `Feedback * f32` — scaling walks a coefficient across the stability
///   bound unchecked. `new_clamped` / `stable_pair` are the ways in.
/// - `Drive * f32` — a shaper's response is nonlinear, so "twice the
///   drive" is not twice anything.
/// - `From<raw> for Unit`, `From<Unit> for raw`, so `.into()` between a unit and
///   its bare number — a bare float must not become a unit unnamed at the call
///   site, and a unit must not leave as a float by an `.into()` that hides which
///   quantity it was. Build with `new`, read with `get` (generic code: the
///   `Unit` trait's `from_raw` / `to_raw`).
///
/// The body is empty and this test cannot fail; see the note in `units/tests.rs`.
#[test]
fn omitted_operators_are_documented() {}

#[test]
fn mix_blends_and_refuses_to_scale() {
    assert_eq!(Mix::DRY.blend(1.0, 9.0), 1.0);
    assert_eq!(Mix::WET.blend(1.0, 9.0), 9.0);
    assert_eq!(Mix(0.5).blend(0.0, 1.0), 0.5);
    assert_eq!(Mix::new_clamped(1.5), Mix::WET);
    assert_eq!(Mix::new_clamped(-0.5), Mix::DRY);
    // `Mix * f32` and `Mix + Mix` are deliberately absent — see the
    // omission ledger.
}

#[test]
fn feedback_stops_below_unity() {
    assert_eq!(Feedback::new_clamped(1.5), Feedback::MAX_STABLE);
    assert_eq!(Feedback::new_clamped(-0.2), Feedback::NONE);
    assert!(
        Feedback::MAX_STABLE.get() < 1.0,
        "a unity loop never decays"
    );
    // The literal this constant replaces, at 13 sites.
    assert_eq!(Feedback::MAX_STABLE, Feedback(0.99));
}

#[test]
fn cross_coupled_feedback_is_bounded_as_a_pair() {
    // The bug a per-value clamp cannot catch: two coefficients that feed
    // the SAME recirculation, each individually legal, summing to 1.98.
    let (d, c) = Feedback::stable_pair(0.99, 0.99);
    assert!(
        d.get() + c.get() <= Feedback::MAX_STABLE.get() + 1e-6,
        "combined feedback {} still runs away",
        d.get() + c.get()
    );
    // Scaled together, so the balance between them survives.
    assert!((d.get() - c.get()).abs() < 1e-6);

    // A pair that is already stable passes through untouched.
    let (d, c) = Feedback::stable_pair(0.5, 0.2);
    assert_eq!((d, c), (Feedback(0.5), Feedback(0.2)));

    // Ratio preserved when scaling is needed.
    let (d, c) = Feedback::stable_pair(0.8, 0.4);
    assert!((d.get() / c.get() - 2.0).abs() < 1e-5);
}

#[test]
fn depth_is_signed_because_inversion_is_the_point() {
    assert_eq!(-Depth::FULL, Depth::INVERTED);
    assert_eq!(Depth::clamped(-2.0), Depth::INVERTED);
    assert_eq!(Depth::clamped(2.0), Depth::FULL);
}

/// A depth is within `-1..=1`: `new`, `TryFrom`, `of` and serde agree, and
/// NaN and the infinities are refused. (Serde is a feature here; a workspace
/// run turns it on through the crates that persist units.)
#[cfg(feature = "serde")]
#[test]
fn depth_keeps_the_range_contract() {
    let c = tutti_fixtures::RangeContract {
        new: Depth::new,
        get: Depth::get,
        of: Some(Depth::of),
        valid: &[-1.0, -0.5, 0.0, 0.25, 1.0],
        invalid: &[-1.001, 1.001, f32::NAN, f32::INFINITY, f32::NEG_INFINITY],
    };
    tutti_fixtures::check_range_contract(&c);
    tutti_fixtures::check_range_serde(&c);
}

/// The one saturating constructor maps NaN to `NONE`, and `Unit::from_raw`
/// (a parameter cell's read) is it: a cell can hold any float and a `Depth`
/// never leaves its range.
#[test]
fn depth_saturates_only_by_name_and_through_a_cell() {
    assert_eq!(Depth::clamped(f32::NAN), Depth::NONE);
    assert_eq!(Depth::clamped(f32::INFINITY), Depth::FULL);
    assert_eq!(Depth::clamped(f32::NEG_INFINITY), Depth::INVERTED);
    assert_eq!(<Depth as Unit>::from_raw(9.0), Depth::FULL);
    assert_eq!(<Depth as Unit>::from_raw(-0.5).get(), -0.5);
}

#[test]
fn drive_is_not_a_gain() {
    // Shares `Amplitude`'s range but not its meaning: `Drive` is never
    // multiplied onto a sample, so it is deliberately not scalable.
    assert_eq!(Drive::UNITY, Drive(1.0));
    assert!(Drive(10.0) > Drive::UNITY);
}

/// `stable_pair` scales a too-large pair down together, keeping its ratio:
/// `0.99 · x / (d + c)` per coefficient, so the sum is the bound.
#[test]
fn stable_pair_keeps_the_ratio_and_survives_infinity() {
    // 1e-6 absolute: a few f32 ulps at these magnitudes (the sums land
    // within 2e-7 of the bound).
    let near = |got: (Feedback, Feedback), want: (f32, f32)| {
        assert!(
            (got.0.get() - want.0).abs() < 1e-6 && (got.1.get() - want.1).abs() < 1e-6,
            "got {got:?}, want {want:?}"
        );
    };
    // Under the bound: untouched, and exactly so.
    assert_eq!(
        Feedback::stable_pair(0.5, 0.25),
        (Feedback(0.5), Feedback(0.25))
    );
    // Over it: 0.99 · 1.5 / 2 and 0.99 · 0.5 / 2.
    near(Feedback::stable_pair(1.5, 0.5), (0.7425, 0.2475));
    // The ratio is kept: 0.99 · 0.9 / 1.8 each.
    near(Feedback::stable_pair(0.9, 0.9), (0.495, 0.495));
    // Negative raises to zero, so the other takes the bound alone.
    near(Feedback::stable_pair(-1.0, 2.0), (0.0, 0.99));
    // An infinity takes the whole bound; two share it.
    near(Feedback::stable_pair(f32::INFINITY, 0.0), (0.99, 0.0));
    near(Feedback::stable_pair(0.3, f32::INFINITY), (0.0, 0.99));
    near(
        Feedback::stable_pair(f32::INFINITY, f32::INFINITY),
        (0.495, 0.495),
    );
    near(Feedback::stable_pair(f32::NEG_INFINITY, 0.5), (0.0, 0.5));
    // A sum that overflows `f32` keeps its ratio too.
    near(Feedback::stable_pair(f32::MAX, f32::MAX), (0.495, 0.495));
    // The sum is the bound to within f32 rounding, even at the extreme.
    let (d, c) = Feedback::stable_pair(0.0, f32::MAX);
    assert!((d.get() + c.get() - 0.99).abs() < 1e-6);
    // NaN stays NaN in its own coefficient; the other clamps alone.
    let (d, c) = Feedback::stable_pair(f32::NAN, 2.0);
    assert!(d.get().is_nan());
    assert_eq!(c, Feedback::MAX_STABLE);
}

/// Without serde the same refusals hold through `new`, `TryFrom` and `of`.
#[test]
fn depth_refuses_what_is_outside_minus_one_to_one() {
    for bad in [-1.001, 1.001, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert_eq!(Depth::new(bad), None, "{bad}");
        assert!(Depth::try_from(bad).is_err(), "{bad}");
        assert!(
            std::panic::catch_unwind(|| Depth::of(bad)).is_err(),
            "{bad}"
        );
    }
    for ok in [-1.0, -0.0, 0.0, 0.5, 1.0] {
        assert_eq!(Depth::new(ok).map(Depth::get), Some(ok));
        assert_eq!(f32::from(Depth::try_from(ok).unwrap()), ok);
    }
}
