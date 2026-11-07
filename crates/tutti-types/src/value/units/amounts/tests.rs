use super::*;

/// What is deliberately *absent* is as load-bearing as what is present.
/// These must not compile; the ledger is here so a future contributor does
/// not "helpfully" add them.
///
/// - `Mix + Mix`, `Mix * f32` — a blend *position* does
///   not compose or scale; half of a crossfade point is not a crossfade
///   point. (`Depth` *is* additive and scalable — that is the difference
///   between a position and a signed magnitude.)
/// - `Feedback * f32` — scaling walks a coefficient across the stability
///   bound unchecked. `new_clamped` / `stable_pair` are the ways in.
/// - `Drive * f32` — a shaper's response is nonlinear, so "twice the
///   drive" is not twice anything.
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
    assert_eq!(Depth::new_clamped(-2.0), Depth::INVERTED);
    assert_eq!(Depth::new_clamped(2.0), Depth::FULL);
    // Additive and scalable, unlike `Mix` and `Feedback`.
    assert_eq!(Depth(0.25) + Depth(0.25), Depth(0.5));
    assert_eq!(Depth::FULL * 0.5, Depth(0.5));
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
///
/// Mutation (run): the scale `MAX_STABLE / total` → `MAX_STABLE / (total +
/// 1.0)` → `(1.5, 0.5)` gives `(0.495, 0.165)` → fails. Mutation (run):
/// drop the `is_infinite` arm → `(INFINITY, 0.0)` gives NaN → fails.
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
