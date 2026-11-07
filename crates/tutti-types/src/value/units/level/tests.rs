use super::*;

/// What is deliberately *absent* is as load-bearing as what is present.
/// These must not compile; the ledger is here so a future contributor does
/// not "helpfully" add them.
///
/// - `Db * f32` — squares the amplitude rather than doubling the gain.
/// - `Db / Db` — a quotient of logarithms is not a quantity.
/// - `Amplitude + Amplitude` — cascading gains multiply; summing is what
///   the *signals* do, not their gains.
///
/// The body is empty and this test cannot fail. It exists as documentation:
/// the omission ledger above is the contract, and a `#[test]` is what keeps
/// it beside the operators rather than drifting into a README. Adding an
/// omitted operator means deleting its entry here, not making this pass.
#[test]
fn omitted_operators_are_documented() {}

#[test]
fn db_round_trips_through_amplitude() {
    assert_eq!(Db::UNITY.to_amplitude(), Amplitude(1.0));
    // -6 dB is very nearly half amplitude.
    assert!((Db(-6.0).to_amplitude().get() - 0.501_187).abs() < 1e-5);
    // +6 dB exceeds 1.0 — amplitude is not a 0..1 quantity.
    assert!(Db(6.0).to_amplitude().get() > 1.99);

    let round = Db::from_amplitude(Db(-12.0).to_amplitude());
    assert!((round.get() - -12.0).abs() < 1e-4);
}

#[test]
fn db_leaves_the_silence_floor_to_its_consumer() {
    // The conversion itself applies no floor. This is the property that
    // makes it a single-answer conversion, and therefore a `From`.
    assert!(Db::from_amplitude(Amplitude(0.0)).get().is_infinite());
    assert!(Db::from(Amplitude::SILENT).get().is_infinite());

    // -inf is what round-trips exactly: 10^(-inf/20) is 0.0, while
    // 10^(-144/20) is merely very small. A floor applied here would lose
    // that.
    assert_eq!(
        Db::from_amplitude(Amplitude::SILENT).to_amplitude(),
        Amplitude::SILENT
    );
    assert!(Db::FLOOR.to_amplitude().get() > 0.0);

    // `Db::FLOOR` still exists — it is what the consumers pin *to*, and
    // the three that do are the dynamics detectors, `ms_ratio`, and the
    // true-peak reading. It is simply not applied by the conversion.
    assert!(Db::FLOOR.get().is_finite());
}

#[test]
fn amplitude_is_not_a_normalized_scale() {
    // A gain routinely exceeds 1.0 — this is not a normalized `0..1`
    // scale. +6 dB is roughly a doubling.
    assert!(Db(6.0).to_amplitude() > Amplitude::UNITY);
    assert_eq!(Db::UNITY.to_amplitude(), Amplitude::UNITY);
    assert_eq!(Db::from(Amplitude::UNITY), Db::UNITY);

    // Scalable, because trimming a gain is meaningful.
    assert_eq!(Amplitude(2.0) * 0.5, Amplitude::UNITY);
}

#[test]
fn db_adds_because_gain_stages_cascade() {
    // Two -6 dB stages in series is -12 dB. This is why `Db` is additive
    // while `Beat` is not.
    assert_eq!(Db(-6.0) + Db(-6.0), Db(-12.0));
    assert_eq!(-Db(6.0), Db(-6.0));
    assert!(Db(-3.0) > Db(-6.0));
}

/// Silence is `-inf`, as `Db::from_amplitude` says, and the figures are the
/// decibel definition: `20 · log10(2) = 6.0206`.
///
/// Mutation (run): make `to_db` pin silence to `Db::FLOOR` → `-inf` becomes
/// `-144` → fails.
#[test]
fn amplitude_to_db_leaves_silence_at_minus_infinity() {
    assert_eq!(Amplitude::SILENT.to_db().get(), f32::NEG_INFINITY);
    assert_eq!(Amplitude::UNITY.to_db(), Db::UNITY);
    assert!((Amplitude(2.0).to_db().get() - 6.0206).abs() < 1e-4);
    assert!((Amplitude(0.5).to_db().get() + 6.0206).abs() < 1e-4);
    assert!((Amplitude(0.1).to_db().get() + 20.0).abs() < 1e-5);
    // The floor is the caller's: it is a different, finite number.
    assert_ne!(Amplitude::SILENT.to_db(), Db::FLOOR);
}

/// The metering floor is 144 dB below full scale, about one 24-bit step
/// (`6.02 dB` a bit, 24 bits).
///
/// Mutation (run): `Db::FLOOR` `-144.0` → `-120.0` → fails both.
#[test]
fn the_metering_floor_is_144_db_down() {
    assert_eq!(Db::FLOOR.get(), -144.0);
    let amp = Db::FLOOR.to_amplitude().get();
    assert!((amp - 6.31e-8).abs() < 1e-9, "{amp:e}");
}
