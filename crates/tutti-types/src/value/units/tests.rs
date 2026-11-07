use super::*;

// Each family's tests hold an omission ledger: an empty
// `omitted_operators_are_documented` test whose doc comment lists the
// operators its types deliberately lack. The body is empty and the test cannot
// fail. It exists as documentation: the ledger is the contract, and a `#[test]`
// is what keeps it beside the operators rather than drifting into a README.
// Adding an omitted operator means deleting its entry, not making a test pass.

#[test]
fn from_is_the_spelling_for_single_answer_conversions() {
    // Each of these delegates to its inherent method, so the two spellings
    // cannot drift.
    assert_eq!(Semitones::from(Cents(150.0)), Cents(150.0).to_semitones());
    assert_eq!(Cents::from(Semitones(1.5)), Semitones(1.5).to_cents());
    assert_eq!(Radians::from(Phase(0.25)), Phase(0.25).to_radians());
    assert_eq!(
        StereoWidth::from(Correlation(0.25)),
        Correlation(0.25).to_stereo_width()
    );
    assert_eq!(Db::from(Amplitude(0.5)), Db::from_amplitude(Amplitude(0.5)));
    assert_eq!(Amplitude::from(Db(-6.0)), Db(-6.0).to_amplitude());

    // And they round-trip where the pair is mutually inverse.
    assert_eq!(Cents::from(Semitones::from(Cents(1200.0))), Cents(1200.0));
}

/// The `From`/named-method split, as a ledger — the same discipline the
/// operator omissions in the family tests get, and for the same reason: a split nobody
/// wrote down drifts unexamined.
#[test]
fn conversions_that_stay_named_and_why() {
    // Needs a second input, so it is not a two-type conversion at all.
    assert_eq!(Seconds(1.0).to_samples(SampleRate::SR_48K), Samples(48_000));

    // What stays named is the case where the second input is real. A
    // policy knob hiding inside a conversion is not that case — it is
    // relocated to the consumer instead, which is what lets
    // `Amplitude` <-> `Db` be a `From`. See the test below.

    // Target is a bare scalar, and two different units convert into it —
    // as `From` impls these would be two meanings of one target type.
    assert!((Cents(1200.0).to_pitch_ratio() - 2.0).abs() < 1e-6);
    assert!((Semitones(12.0).to_pitch_ratio() - 2.0).abs() < 1e-6);

    // `ArcDegrees` has no radians conversion at all. `Azimuth` and
    // `Elevation` do (as `From`), so adding one here would look like
    // filling a gap — but nothing needs it, and an unused conversion
    // between a displacement and an angle invites treating one as the
    // other. Left absent deliberately.
}

#[test]
fn clamp_stays_in_the_unit_type() {
    assert_eq!(Hz(20_000.0).clamp(Hz(20.0), Hz(18_000.0)), Hz(18_000.0));
    assert_eq!(Mix(1.5).clamp(Mix::DRY, Mix::WET), Mix::WET);
}

/// The module's one non-finite rule, for every `new_clamped`: NaN stays
/// NaN, an infinity lands on the bound on its side, and an unbounded side
/// stays infinite.
///
/// Mutation (run): write `StereoWidth::new_clamped` as `v.max(0.0)` again
/// → NaN reads as `0.0` → fails; give `Mix::new_clamped` a
/// `if v.is_nan() { 0.0 }` arm → fails.
#[test]
fn every_new_clamped_passes_nan_and_saturates_infinity() {
    let nan = f32::NAN;
    let (inf, ninf) = (f32::INFINITY, f32::NEG_INFINITY);
    macro_rules! check {
        ($t:ident, $lo:expr, $hi:expr) => {
            assert!($t::new_clamped(nan).get().is_nan(), stringify!($t));
            assert_eq!($t::new_clamped(inf).get(), $hi, stringify!($t));
            assert_eq!($t::new_clamped(ninf).get(), $lo, stringify!($t));
        };
    }
    check!(Mix, 0.0, 1.0);
    check!(Feedback, 0.0, 0.99);
    check!(Depth, -1.0, 1.0);
    check!(Spread, 0.0, 1.0);
    check!(Resonance, 0.0, 1.0);
    check!(Confidence, 0.0, 1.0);
    check!(Correlation, -1.0, 1.0);
    check!(Pan, -1.0, 1.0);
    check!(Velocity, 0.0, 1.0);
    check!(Elevation, -90.0, 90.0);
    check!(PlaybackRate, 0.25, 4.0);
    check!(StretchFactor, 0.25, 4.0);
    // The unbounded side stays infinite.
    check!(StereoWidth, 0.0, f32::INFINITY);
    check!(CompressionRatio, 1.0, f32::INFINITY);
    check!(Q, f32::MIN_POSITIVE, f32::INFINITY);
    // In-range values pass untouched, and -0.0 keeps its sign bit.
    assert_eq!(StereoWidth::new_clamped(1.5), StereoWidth(1.5));
    assert_eq!(CompressionRatio::new_clamped(4.0), CompressionRatio(4.0));
    assert_eq!(Q::new_clamped(0.7), Q(0.7));
    assert_eq!(
        StereoWidth::new_clamped(-0.0).get().to_bits(),
        (-0.0f32).to_bits()
    );
}

/// Tiny negatives are within rounding of a whole cycle below zero, and the
/// documented half-open range holds for them.
///
/// Mutation (run): drop the `r >= 1.0` guard in `Phase::wrapped` →
/// `-1e-9` gives `1.0` → fails. Drop the `r >= 360.0` guard in
/// `Azimuth::wrap` → `-180.00002` gives `180.0` → fails.
#[test]
fn the_wraps_hold_their_half_open_range_at_the_seam() {
    let below_one = 1.0_f32.next_down();
    for v in [-1e-9_f32, -1e-30, -f32::MIN_POSITIVE, -2.9e-8, -5.9e-8] {
        let p = Phase::wrapped(v);
        assert!((0.0..1.0).contains(&p.get()), "wrapped({v:e}) = {p}");
        assert_eq!(p, Phase(below_one), "{v:e}");
    }
    // Ordinary inputs keep their exact figures.
    assert_eq!(Phase::wrapped(-0.25), Phase(0.75));
    assert_eq!(Phase::wrapped(0.0), Phase(0.0));
    assert_eq!(Phase::wrapped(1.0), Phase(0.0));
    assert_eq!(Phase::wrapped(2.0), Phase(0.0));
    assert_eq!(Phase::wrapped(-1.0), Phase(0.0));
    // NaN and the infinities give NaN.
    for v in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert!(Phase::wrapped(v).get().is_nan());
    }

    let top = 360.0_f32.next_down() - 180.0;
    for v in [-180.00002_f32, -180.000_01, -180.000_000_1] {
        let a = Azimuth(v).wrap().get();
        assert!((-180.0..180.0).contains(&a), "{v} wrapped to {a}");
    }
    assert_eq!(Azimuth(-180.00002).wrap(), Azimuth(top));
    assert_eq!(Azimuth(190.0).wrap(), Azimuth(-170.0));
    assert_eq!(Azimuth(-190.0).wrap(), Azimuth(170.0));
    assert_eq!(Azimuth(180.0).wrap(), Azimuth(-180.0));
    assert_eq!(Azimuth(-180.0).wrap(), Azimuth(-180.0));
    assert!(Azimuth(f32::NAN).wrap().get().is_nan());
    assert!(Azimuth(f32::INFINITY).wrap().get().is_nan());

    assert!(Radians(f32::NAN).wrapped_signed().get().is_nan());
    assert!(Radians(f32::INFINITY).wrapped_signed().get().is_nan());

    let below_one = 1.0_f64.next_down();
    assert_eq!(Beat(-1e-20).fract(), BeatDuration(below_one));
    assert_eq!(SamplePosition(-1e-20).fract(), below_one);
    assert_eq!(Beat(2.25).fract(), BeatDuration(0.25));
    assert_eq!(Beat(-0.75).fract(), BeatDuration(0.25));
    assert_eq!(Beat(3.0).fract(), BeatDuration(0.0));
    assert!(Beat(f64::NAN).fract().get().is_nan());
    assert!(Beat(f64::INFINITY).fract().get().is_nan());

    let four = BeatDuration(4.0);
    assert_eq!(
        BeatDuration(-1e-20).rem_euclid(four),
        BeatDuration(4.0_f64.next_down())
    );
    assert_eq!(BeatDuration(-1.0).rem_euclid(four), BeatDuration(3.0));
    assert_eq!(BeatDuration(9.0).rem_euclid(four), BeatDuration(1.0));
    assert_eq!(
        BeatDuration(-1e-20).rem_euclid(BeatDuration(-4.0)),
        BeatDuration(4.0_f64.next_down())
    );
    assert!(
        BeatDuration(1.0)
            .rem_euclid(BeatDuration(0.0))
            .get()
            .is_nan()
    );
}
