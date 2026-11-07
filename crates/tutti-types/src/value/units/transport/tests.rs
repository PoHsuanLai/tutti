use super::*;

/// What is deliberately *absent* is as load-bearing as what is present.
/// These must not compile; the ledger is here so a future contributor does
/// not "helpfully" add them.
///
/// - `Beat + Beat` — adding two timeline positions has no meaning.
/// - `Beat * f64` — scaling a position depends on where zero is.
/// - `Beat % BeatDuration` — hides the loop-start origin.
/// - `Bpm - Bpm` — a tempo difference is not a tempo.
/// - `ReadRate * f64`, `ReadRate + ReadRate` — `advance`, which names the
///   operand as a sample count and returns a `SamplePosition`. This type
///   exists to be multiplied by exactly one thing, so a bare `*` would let
///   it be multiplied by anything — including the `PlaybackRate` and
///   `SrcRatio` it is already the product of, which would apply a ratio twice.
///
/// And a beat-synced rate is a [`BeatDuration`] (beats per cycle), never an
/// [`Hz`]: the two are *inverse*. Doubling
/// the span halves the rate, so the substitution is a silent reciprocal.
/// [`Beat::cycles_of`] is the only interpreter.
///
/// The body is empty and this test cannot fail. It exists as documentation:
/// the omission ledger above is the contract, and a `#[test]` is what keeps
/// it beside the operators rather than drifting into a README. Adding an
/// omitted operator means deleting its entry here, not making this pass.
#[test]
fn omitted_operators_are_documented() {}

/// The affine discipline: subtracting two positions yields a *displacement*,
/// and only a displacement can be added back to a position.
#[test]
fn beat_and_beat_duration_form_an_affine_pair() {
    let a = Beat(4.0);
    let b = Beat(6.5);

    let span: BeatDuration = b - a;
    assert_eq!(span, BeatDuration(2.5));

    assert_eq!(a + span, b);
    assert_eq!(b - span, a);

    let mut cursor = Beat(0.0);
    cursor += BeatDuration(0.25);
    assert_eq!(cursor, Beat(0.25));
}

#[test]
fn sample_position_advances_by_a_frame_count_without_losing_the_fraction() {
    // The named replacement for the forbidden `SamplePosition + Samples`.
    // Exactness is the whole point: an interpolating reader sits between two
    // integer indices, and advancing must not quietly seat it on one.
    let entry = SamplePosition(1000.5);
    assert_eq!(entry.advanced_by(Samples(8192)), SamplePosition(9192.5));
    assert_eq!(entry.advanced_by(Samples(0)), entry);
}

#[test]
fn sample_position_to_samples_names_its_rounding() {
    // The distinction that makes these two methods rather than one: a
    // preload window ending mid-frame must load THAT frame, so a window
    // sized by `floor` under-loads and the reader interpolates against data
    // it does not have.
    let mid_frame = SamplePosition(9192.5);
    assert_eq!(mid_frame.to_samples_floor(), Samples(9192));
    assert_eq!(mid_frame.to_samples_ceil(), Samples(9193));

    // Exact positions agree — the rounding only bites on the fraction.
    let exact = SamplePosition(9192.0);
    assert_eq!(exact.to_samples_floor(), exact.to_samples_ceil());

    // Ordering holds for every input, which is the invariant a caller
    // reasons with when it picks one.
    for raw in [0.0, 0.25, 1.0, 1.75, 48_000.5, 1e9 + 0.5] {
        let p = SamplePosition(raw);
        assert!(p.to_samples_ceil() >= p.to_samples_floor(), "{raw}");
    }
}

#[test]
fn sample_position_to_samples_collapses_nonsense_to_zero() {
    // Matches `Seconds::to_samples`'s convention rather than inventing a
    // second one: a negative or non-finite position has no frames behind it.
    assert_eq!(SamplePosition(-1.0).to_samples_floor(), Samples::ZERO);
    assert_eq!(SamplePosition(-1.0).to_samples_ceil(), Samples::ZERO);
    assert_eq!(SamplePosition(f64::NAN).to_samples_floor(), Samples::ZERO);
    assert_eq!(SamplePosition(f64::NAN).to_samples_ceil(), Samples::ZERO);
}

#[test]
fn sample_position_survives_a_long_file() {
    // The reason `SamplePosition` is f64: f32 holds integers exactly only to
    // 2^24, ~5.8 minutes at 48k. A loop point in an hour-long file must not
    // snap, and neither must a preload window computed against one.
    let hour_in = SamplePosition(48_000.0 * 3600.0);
    assert_eq!(
        hour_in.advanced_by(Samples(8192)).to_samples_floor(),
        Samples(172_800_000 + 8192)
    );
}

#[test]
fn beat_duration_to_seconds_is_the_readout_conversion() {
    // At 120 BPM a beat is half a second.
    assert_eq!(BeatDuration(1.0).to_seconds(Bpm(120.0)), Seconds(0.5));
    assert_eq!(BeatDuration(4.0).to_seconds(Bpm(120.0)), Seconds(2.0));
    assert_eq!(BeatDuration(1.0).to_seconds(Bpm(60.0)), Seconds(1.0));
    // A degenerate tempo yields zero rather than inf.
    assert_eq!(BeatDuration(1.0).to_seconds(Bpm(0.0)), Seconds(0.0));
}

#[test]
fn beat_splits_into_floor_and_fraction() {
    let b = Beat(4.75);
    assert_eq!(b.floor(), Beat(4.0));
    assert_eq!(b.fract(), BeatDuration(0.75));
    // The two halves recombine, and are visibly different kinds of thing.
    assert_eq!(b.floor() + b.fract(), b);
}

#[test]
fn cycles_of_reads_the_divisor_as_a_span_not_a_frequency() {
    // The reciprocal that shipped twice: a *longer* span is a *slower*
    // source. Under the Hz reading, 2.0 would be twice as fast and land on
    // cycle 8 here.
    assert_eq!(Beat(4.0).cycles_of(BeatDuration(2.0)), Some(2.0));
    assert_eq!(Beat(4.0).cycles_of(BeatDuration(0.5)), Some(8.0));

    // Un-wrapped: the integer part is the cycle index a stepped shape
    // addresses itself by, the fraction is the phase.
    let c = Beat(5.0).cycles_of(BeatDuration(2.0)).unwrap();
    assert_eq!(c.floor(), 2.0);
    assert_eq!(c.fract(), 0.5);

    // One guard, replacing three that disagreed — a non-positive span
    // froze on one path and ran backwards on another.
    assert_eq!(Beat(4.0).cycles_of(BeatDuration(0.0)), None);
    assert_eq!(Beat(4.0).cycles_of(BeatDuration(-2.0)), None);
}

#[test]
fn cycles_of_stays_in_f64_where_the_callers_used_to_narrow() {
    // Every call site divided `beat as f32` by an f32 span. That is exact
    // for power-of-two spans, which is why it went unnoticed, and drifts on
    // everything else.
    let beat = Beat(1_000_000.0);
    let span = BeatDuration(3.0);
    let wide = beat.cycles_of(span).unwrap();
    let narrowed = ((beat.get() as f32) / (span.get() as f32)) as f64;
    assert!(
        (wide.fract() - narrowed.fract()).abs() > 0.01,
        "the narrowing this method exists to avoid should be measurable here"
    );
    assert_eq!(wide, 1_000_000.0 / 3.0);
}

#[test]
fn beat_duration_ratio_counts_how_many_fit() {
    // "how many samples fit in this beat span" — the dimensional analysis
    // a rate type would otherwise be invented for.
    let span = BeatDuration(1.0);
    let per_sample = BeatDuration(0.25);
    let n: f64 = span / per_sample;
    assert_eq!(n, 4.0);
}

#[test]
fn rem_euclid_wraps_negative_overshoot_inside_the_span() {
    let len = BeatDuration(4.0);
    // Plain `%` keeps the dividend's sign, landing outside the span.
    assert_eq!(BeatDuration(-1.0) % len, BeatDuration(-1.0));
    // `rem_euclid` is the wrapping-safe form.
    assert_eq!(BeatDuration(-1.0).rem_euclid(len), BeatDuration(3.0));
}

#[test]
fn bpm_differs_from_ignores_ulp_noise() {
    let a = Bpm(120.0);
    assert!(!a.differs_from(Bpm(120.000_000_1), 0.001));
    assert!(a.differs_from(Bpm(140.0), 0.001));
}

#[test]
fn src_ratio_derives_from_typed_rates() {
    // `for_rates` is the one place the derivation lives, and it now speaks
    // the same type its two arguments are measured in.
    assert_eq!(
        SrcRatio::for_rates(SampleRate::SR_44K1, SampleRate::SR_44K1),
        SrcRatio::UNITY
    );
    let r = SrcRatio::for_rates(SampleRate::SR_48K, SampleRate::SR_44K1);
    assert!((r.get() - 48_000.0 / 44_100.0).abs() < 1e-6);
}

#[test]
fn src_ratio_is_exactly_unity_for_matched_rates() {
    // Bit-exact, not 1.0000001 — the matched case is the common one and must
    // not accumulate resampling error.
    assert_eq!(SrcRatio::for_rates(44_100.0, 44_100.0), SrcRatio::UNITY);
    // Within the 0.01 Hz tolerance.
    assert_eq!(SrcRatio::for_rates(44_100.005, 44_100.0), SrcRatio::UNITY);
}

#[test]
fn src_ratio_converts_mismatched_rates() {
    let r = SrcRatio::for_rates(48_000.0, 44_100.0);
    assert!((r.get() - 48_000.0 / 44_100.0).abs() < 1e-6);
    assert!(
        r > SrcRatio::UNITY,
        "a faster file reads more source samples"
    );
}

#[test]
fn src_ratio_survives_a_nonpositive_session_rate() {
    // Would otherwise divide by zero and poison every downstream sample.
    assert_eq!(SrcRatio::for_rates(44_100.0, 0.0), SrcRatio::UNITY);
    assert_eq!(SrcRatio::for_rates(44_100.0, -1.0), SrcRatio::UNITY);
}

#[test]
fn playback_rate_clamps_into_range() {
    assert_eq!(PlaybackRate::new_clamped(8.0), PlaybackRate::MAX);
    assert_eq!(PlaybackRate::new_clamped(0.0), PlaybackRate::MIN);
    assert_eq!(PlaybackRate::new_clamped(-2.0), PlaybackRate::MIN);
    assert_eq!(PlaybackRate::new_clamped(1.5), PlaybackRate::new(1.5));
}

/// The module's one non-finite rule: NaN stays NaN, an infinity saturates
/// at its bound.
#[test]
fn playback_rate_follows_the_non_finite_rule() {
    assert!(PlaybackRate::new_clamped(f32::NAN).get().is_nan());
    assert_eq!(PlaybackRate::new_clamped(f32::INFINITY), PlaybackRate::MAX);
    assert_eq!(
        PlaybackRate::new_clamped(f32::NEG_INFINITY),
        PlaybackRate::MIN
    );
}

#[test]
fn read_rate_composes_varispeed_with_conversion() {
    // Half speed on a 48k file in a 44.1k session: both effects apply once.
    // Tolerance is f32-scale, not f64: both rates are f32-backed, so the
    // quotient is rounded once on the way in. Widening to f64 in `read_rate`
    // keeps the *accumulation* exact (a read position advanced a million
    // times must not drift), but it cannot recover precision already lost.
    let rate = PlaybackRate::new(0.5);
    let src = SrcRatio::for_rates(48_000.0, 44_100.0);
    let expected = 0.5 * (48_000.0 / 44_100.0);
    assert!((rate.read_rate(src).get() - expected).abs() < 1e-6);

    // Unity on both sides consumes exactly one source sample per output —
    // this one IS exact, and must stay so: it is the matched-rate path every
    // same-sample-rate session takes.
    assert_eq!(
        PlaybackRate::UNITY.read_rate(SrcRatio::UNITY),
        ReadRate::UNITY
    );
}

/// `ReadRate` omits `Mul`, so `advance` is the only way to turn a rate into a
/// distance — the omission ledger's rule that a removed operator ships with
/// its named replacement.
#[test]
fn read_rate_advances_a_position_by_a_sample_count() {
    // Half speed: 512 output samples consume 256 source samples.
    let half = PlaybackRate::new(0.5).read_rate(SrcRatio::UNITY);
    assert_eq!(half.advance(Samples(512)), SamplePosition(256.0));

    // Unity is the identity, exactly.
    assert_eq!(
        ReadRate::UNITY.advance(Samples(1024)),
        SamplePosition(1024.0)
    );

    // Zero output samples advance nowhere, whatever the rate.
    assert_eq!(half.advance(Samples(0)), SamplePosition(0.0));
}

/// `then` composes two read rates applied in series, and composition must be
/// exactly multiplication — a stretched varispeed voice reads at the product.
#[test]
fn read_rates_compose_in_series() {
    let half = PlaybackRate::new(0.5).read_rate(SrcRatio::UNITY);
    let double = PlaybackRate::new(2.0).read_rate(SrcRatio::UNITY);

    // Half speed then double: back to unity, and exactly so.
    assert_eq!(half.then(double), ReadRate::UNITY);

    // Unity is the identity on both sides.
    assert_eq!(half.then(ReadRate::UNITY), half);
    assert_eq!(ReadRate::UNITY.then(half), half);

    // Order does not matter — the composition is a product.
    assert_eq!(half.then(double), double.then(half));

    // The motivating case: a 2x-stretched voice at half varispeed reads a
    // quarter of a source sample per output sample. Composed, not
    // hand-multiplied — which is the whole point of the method.
    let stretch_half = ReadRate(0.5);
    assert_eq!(half.then(stretch_half), ReadRate(0.25));
    assert_eq!(
        half.then(stretch_half).advance(Samples(1024)),
        SamplePosition(256.0)
    );
}

#[test]
fn stretch_factor_clamps_and_follows_the_non_finite_rule() {
    assert_eq!(StretchFactor::new_clamped(8.0), StretchFactor::MAX);
    assert_eq!(StretchFactor::new_clamped(0.1), StretchFactor::MIN);
    assert!(StretchFactor::new_clamped(f32::NAN).get().is_nan());
    assert_eq!(
        StretchFactor::new_clamped(f32::INFINITY),
        StretchFactor::MAX
    );
    assert_eq!(
        StretchFactor::new_clamped(f32::NEG_INFINITY),
        StretchFactor::MIN
    );
}
