use super::*;

/// What is deliberately *absent* is as load-bearing as what is present.
/// These must not compile; the ledger is here so a future contributor does
/// not "helpfully" add them.
///
/// - `From<raw> for Unit`, `From<Unit> for raw`, so `.into()` between a unit and
///   its bare number — a bare float must not become a unit unnamed at the call
///   site, and a unit must not leave as a float by an `.into()` that hides which
///   quantity it was. Build with `new`, read with `get` (generic code: the
///   `Unit` trait's `from_raw` / `to_raw`).
///
/// The body is empty and this test cannot fail; see the note in `units/tests.rs`.
#[test]
fn omitted_conversions_are_documented() {}

#[test]
fn cents_and_semitones_convert_rather_than_scale() {
    // `Cents` being `Scalable` means `Cents / 100.0` COMPILES and
    // yields `Cents` — wrong by 100x with a type that says otherwise.
    // These are the forms that carry the unit change.
    assert_eq!(Cents(100.0).to_semitones(), Semitones(1.0));
    assert_eq!(Cents::SEMITONE.to_semitones(), Semitones(1.0));
    assert_eq!(Semitones(1.0).to_cents(), Cents(100.0));
    assert_eq!(Semitones::OCTAVE.to_cents(), Cents(1200.0));

    // An octave doubles the frequency, by either spelling.
    assert!((Semitones::OCTAVE.to_pitch_ratio() - 2.0).abs() < 1e-6);
    assert!((Cents(1200.0).to_pitch_ratio() - 2.0).abs() < 1e-6);
    assert!((Semitones(0.0).to_pitch_ratio() - 1.0).abs() < 1e-6);
}

/// `Cents::from_pitch_ratio` is the named inverse of `to_pitch_ratio`.
///
/// It exists because `tuning.rs` was hand-rolling `1200.0 * ratio.log2()`
/// while the forward direction was already a converter — the asymmetry the
/// omission rule is meant to catch.
#[test]
fn a_pitch_ratio_round_trips_through_cents() {
    for ratio in [0.5_f32, 1.0, 1.5, 2.0, 3.0] {
        let back = Cents::from_pitch_ratio(ratio).to_pitch_ratio();
        assert!(
            (back - ratio).abs() < 1e-4,
            "{ratio} round-tripped to {back}"
        );
    }
    assert_eq!(Cents::from_pitch_ratio(2.0), Cents(1200.0));
    assert_eq!(Cents::from_pitch_ratio(1.0), Cents(0.0));

    // A ratio has no logarithm at or below zero. Unison, not NaN — a NaN
    // here reaches a frequency table and silences a note with no error.
    assert_eq!(Cents::from_pitch_ratio(0.0), Cents(0.0));
    assert_eq!(Cents::from_pitch_ratio(-1.0), Cents(0.0));
}

#[test]
fn the_three_ratios_have_disjoint_ranges() {
    // Which is why one `Ratio` could not serve all three: a legal value of
    // one is an illegal value of another.
    assert_eq!(CompressionRatio::new_clamped(0.5), CompressionRatio::UNITY);
    assert_eq!(CompressionRatio::new_clamped(4.0), CompressionRatio(4.0));

    assert_eq!(Resonance::new_clamped(4.0), Resonance::SELF_OSCILLATION);
    assert_eq!(Resonance::new_clamped(-1.0), Resonance::NONE);

    // `Q` only has to stay above zero — it divides into the damping term.
    assert!(Q::new_clamped(0.0).get() > 0.0);
    assert!(Q::new_clamped(-5.0).get() > 0.0);
    assert_eq!(Q::new_clamped(10.0), Q(10.0));

    // The sharp case: 1.0 means "mild bell" as a Q and "self-oscillating"
    // as a ladder resonance. Same number, different instrument.
    assert!(Q(1.0) > Q::BUTTERWORTH);
    assert_eq!(Resonance(1.0), Resonance::SELF_OSCILLATION);
}
