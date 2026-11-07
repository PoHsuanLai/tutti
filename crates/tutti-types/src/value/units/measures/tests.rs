use super::*;

/// What is deliberately *absent* is as load-bearing as what is present.
/// These must not compile; the ledger is here so a future contributor does
/// not "helpfully" add them.
///
/// - `Confidence + Confidence` — two 0.6-confident estimates do not make a
///   1.2-confident one. `combine` is the replacement, and its name states
///   the independence precondition that multiplying them requires.
/// - `Correlation * f32` — the value is a normalized inner product, so
///   rescaling denormalizes it and it stops meaning "coherence".
/// - `Correlation + Correlation` — coherence does not accumulate;
///   time-averaging it is meter ballistics, which smooths.
/// - `Pan * f32`, `Pan + Pan` — a position on a segment, so the same
///   argument `Mix` makes: half a pan position is not a pan position, and
///   two of them do not sum. Interpolation is a `lerp`, not a `*`.
/// - `Velocity * f32` — `scaled`, which clamps. The bare operator reads as
///   "half as loud", and velocity→loudness is the instrument's mapping, not
///   arithmetic's: a patch may be louder at low velocity.
/// - `Velocity + Velocity` — two strikes do not sum into a harder one.
///
/// And five *type* omissions, which the compiler enforces rather than a
/// missing `impl`: `Correlation` and `Pan` are not `Depth`,
/// `Confidence` is not `Mix`, and `Velocity` is not `Amplitude`, even
/// though the ranges coincide. A control you set and a measurement
/// reported back are different quantities — the same split `Q` and
/// `Resonance` make. `Velocity` is the sharpest of these, because
/// `sample * velocity` is not just meaningless but *plausible*: it
/// compiles as a volume dip and sounds like a quiet note.
///
/// The body is empty and this test cannot fail; see the note in `units/tests.rs`.
#[test]
fn omitted_operators_are_documented() {}

/// Velocity scales by a *named* method, and the scale clamps.
///
/// `scaled` exists because a "soften the take" edit is real, and without a
/// replacement for the omitted `Mul<f32>` every call site would escape to
/// `Velocity(v.get() * 0.8)` — which does not clamp, and which is how a
/// velocity leaves `0..1` and reaches the 16-bit widening out of range.
#[test]
fn velocity_scales_by_name_and_stays_in_range() {
    assert_eq!(Velocity(0.8).scaled(0.5), Velocity(0.4));
    assert_eq!(Velocity(0.8).scaled(4.0), Velocity::MAX);
    assert_eq!(Velocity(0.8).scaled(-1.0), Velocity::SILENT);
    assert_eq!(Velocity::new_clamped(1.7), Velocity::MAX);
}

/// The MIDI 2.0 center code is mezzo-forte, not "half as loud".
///
/// Pinned because `CENTER` looks like an arbitrary 0.5 until you know it is
/// the value `0x8000` widens from — the one point the spec's Min-Center-Max
/// scaler is required to preserve exactly.
#[test]
fn the_center_velocity_is_the_midi_center_code() {
    assert_eq!(Velocity::CENTER, Velocity(0.5));
    assert!(Velocity::SILENT < Velocity::CENTER);
    assert!(Velocity::CENTER < Velocity::MAX);
}

#[test]
fn confidence_combines_rather_than_sums() {
    // Independent estimates multiply into a joint probability — and the
    // result is never larger than either input, which `+` would not
    // preserve.
    let joint = Confidence(0.6).combine(Confidence(0.5));
    assert_eq!(joint, Confidence(0.3));
    assert!(joint <= Confidence(0.6));

    // The guard the shipped pitch detector lacked: YIN's aperiodicity can
    // leave the unit interval, and only the lower bound was clamped.
    assert_eq!(Confidence::new_clamped(1.4), Confidence::CERTAIN);
    assert_eq!(Confidence::new_clamped(-0.2), Confidence::NONE);
}

#[test]
fn correlation_derives_width_rather_than_storing_it() {
    // The invariant the shipped meter broke by storing `width` alongside
    // `correlation` and smoothing the two independently.
    assert_eq!(Correlation::MONO.to_stereo_width(), StereoWidth::MONO);
    assert_eq!(
        Correlation::UNCORRELATED.to_stereo_width(),
        StereoWidth::NATURAL
    );
    assert_eq!(Correlation::INVERTED.to_stereo_width(), StereoWidth(2.0));

    // Phase trouble is a named threshold, not a bare comparison at the
    // call site.
    assert!(Correlation(-0.5).has_phase_issues());
    assert!(!Correlation(-0.1).has_phase_issues());
    assert!(!Correlation::MONO.has_phase_issues());
}

#[test]
fn pan_saturates_and_orders() {
    // Unlike `Azimuth`, this is a segment: past the end it stops rather
    // than wrapping around to the opposite side.
    assert_eq!(Pan::new_clamped(1.5), Pan::RIGHT);
    assert_eq!(Pan::new_clamped(-1.5), Pan::LEFT);

    // And unlike `Azimuth`, it orders — a segment has ends.
    assert!(Pan::LEFT < Pan::CENTER);
    assert!(Pan::CENTER < Pan::RIGHT);
    assert_eq!(-Pan::LEFT, Pan::RIGHT);
}
