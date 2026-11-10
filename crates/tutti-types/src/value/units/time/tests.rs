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
fn seconds_to_samples_names_its_rounding() {
    // 0.5 s at 44100 is exactly 22050 frames — every variant agrees.
    assert_eq!(
        Seconds(0.5).to_samples(SampleRate::new(44_100.0)),
        Samples(22_050)
    );
    assert_eq!(
        Seconds(0.5).to_samples_floor(SampleRate::new(44_100.0)),
        Samples(22_050)
    );
    assert_eq!(
        Seconds(0.5).to_samples_ceil(SampleRate::new(44_100.0)),
        Samples(22_050)
    );

    // 10 ms at 44100 is 441.0; nudge it so the three diverge.
    let odd = Seconds(0.010_5);
    assert_eq!(
        odd.to_samples_floor(SampleRate::new(44_100.0)),
        Samples(463)
    );
    assert_eq!(odd.to_samples_ceil(SampleRate::new(44_100.0)), Samples(464));
    assert_eq!(odd.to_samples(SampleRate::new(44_100.0)), Samples(463));

    // Allocation must never under-size: ceil is >= nearest, always.
    for ms in 1..200 {
        let s = Seconds(ms as f64 * 0.001);
        assert!(
            s.to_samples_ceil(SampleRate::new(48_000.0)) >= s.to_samples(SampleRate::new(48_000.0))
        );
        assert!(
            s.to_samples(SampleRate::new(48_000.0))
                >= s.to_samples_floor(SampleRate::new(48_000.0))
        );
    }
}

#[test]
fn seconds_to_samples_collapses_nonsense_to_zero() {
    // `NaN as usize` is already 0 and a negative cast already saturates —
    // this pins the behaviour so it is stated rather than inherited.
    assert_eq!(
        Seconds(f64::NAN).to_samples(SampleRate::new(48_000.0)),
        Samples::ZERO
    );
    assert_eq!(
        Seconds(f64::INFINITY).to_samples(SampleRate::new(48_000.0)),
        Samples::ZERO
    );
    assert_eq!(
        Seconds(-1.0).to_samples(SampleRate::new(48_000.0)),
        Samples::ZERO
    );
    assert_eq!(Seconds(1.0).to_samples(SampleRate::new(0.0)), Samples::ZERO);
}

#[test]
fn seconds_to_samples_computes_wide_enough_for_a_long_render() {
    // f32 cannot represent frame counts past 2^24 (~6 min at 48k), which
    // is why the arithmetic widens to f64 before rounding.
    let hour = Seconds(3600.0);
    assert_eq!(
        hour.to_samples(SampleRate::new(48_000.0)),
        Samples(172_800_000)
    );
}

#[test]
fn sample_rate_carries_rates_f32_would_round() {
    // The reason the unit is f64-backed. 192 kHz is exact in f32, so the
    // integer value is not the hazard — the reciprocal is: a per-sample
    // step computed in f32 carries ~6e-8 of relative error, which a phase
    // accumulator integrates into audible drift over a long render.
    let sr = SampleRate(192_000.0);
    let step_f64 = 1.0_f64 / sr.get();
    let step_f32 = (1.0_f32 / sr.get() as f32) as f64;
    assert!(
        (step_f64 - step_f32).abs() > 0.0,
        "if f32 were exact here the f64 backing would be unmotivated"
    );
}

#[test]
fn sample_rate_accepts_every_shape_a_caller_holds() {
    // File headers and device configs hand out u32; the engine's own
    // arithmetic is f64. Both widen exactly, which is what lets the
    // migrated signatures take `impl Into<SampleRate>` without churning
    // their call sites.
    assert_eq!(SampleRate::new(48_000.0_f64), SampleRate::SR_48K);
    assert_eq!(SampleRate::from(48_000_u32), SampleRate::SR_48K);
    assert_eq!(SampleRate::SR_44K1, SampleRate(44_100.0));
}

#[test]
fn sample_rate_orders_and_bounds_but_does_not_compose() {
    assert!(SampleRate::SR_44K1 < SampleRate::SR_48K);
    assert_eq!(
        SampleRate::SR_44K1.max(SampleRate::SR_48K),
        SampleRate::SR_48K
    );
    assert_eq!(
        SampleRate(192_000.0).clamp(SampleRate::SR_44K1, SampleRate::SR_48K),
        SampleRate::SR_48K
    );
    // Deliberately absent, per the omission ledger beside the definition:
    // `SR_44K1 + SR_48K` (not a rate), `SR_48K * 2.0` (a scaled rate is a
    // different *device* rate), and `SR_48K / SR_44K1` (that quotient is a
    // `SrcRatio`, and `for_rates` owns the derivation plus its guards).
}

/// **A one-pole smoother built from `one_pole_coeff` reaches `1 − e⁻¹`
/// of a unit step after `τ` seconds**, stepped per sample at `rate` and
/// per block through `one_pole_coeff_over`.
///
/// Mutations (run): `exp(-n)` for `exp(-1 / n)` → fails; returning the
/// retention `exp(−1/n)` instead of `1 − exp(−1/n)` → fails; the
/// step form dividing `step / τ` instead of `τ / step` → fails.
#[test]
fn one_pole_step_response_reaches_one_minus_inverse_e_after_tau() {
    let want = 1.0 - (-1.0f64).exp();
    for (tau, rate) in [(0.01, 48_000.0), (0.5, 44_100.0), (2.0, 96_000.0)] {
        let c = Seconds(tau).one_pole_coeff(SampleRate(rate));
        let n = Seconds(tau).to_samples(SampleRate(rate)).get();
        let mut y = 0.0f64;
        for _ in 0..n {
            y += f64::from(c) * (1.0 - y);
        }
        assert!((y - want).abs() < 1e-4, "τ {tau} at {rate}: {y}");
    }
    // Stepped once per 10 ms block, τ = 100 ms: ten steps.
    let c = Seconds(0.1).one_pole_coeff_over(Seconds(0.01));
    let mut y = 0.0f64;
    for _ in 0..10 {
        y += f64::from(c) * (1.0 - y);
    }
    assert!((y - want).abs() < 1e-5, "per block: {y}");
}

/// **A zero, negative or NaN time constant is instant (1); an infinite
/// one holds (0); a step form's empty step moves nothing.** And a long
/// time constant keeps a coefficient above zero.
///
/// Also carries the deleted `tutti-nodes` `a_zero_smoothing_time_is_instant`
/// (run: `one_pole`'s `else` returning 0, the freeze, fails this test).
///
/// Mutations (run): `one_pole` without its `n > 0.0` guard → zero
/// gives `1 − exp(−inf)` = 1 still, but a negative τ gives
/// `1 − exp(+…)` < 0 and NaN gives NaN → fails; its `else` returning 0
/// (freeze) → fails; `one_pole_coeff_over` without its step guard →
/// a zero step goes through `τ / 0` = inf, a negative one through a
/// negative decay length (instant) → fails.
#[test]
fn one_pole_zero_time_is_instant_and_bad_input_is_defined() {
    let sr = SampleRate::SR_48K;
    for tau in [0.0, -0.0, -1.0, f64::NAN, f64::NEG_INFINITY] {
        assert_eq!(Seconds(tau).one_pole_coeff(sr), 1.0, "τ {tau}");
        assert_eq!(Seconds(tau).one_pole_coeff_over(Seconds(0.01)), 1.0);
        assert_eq!(Seconds(tau).one_pole_coeff_over(Seconds(0.0)), 1.0);
    }
    assert_eq!(Seconds(f64::INFINITY).one_pole_coeff(sr), 0.0);
    assert_eq!(Seconds(1.0).one_pole_coeff(SampleRate(f64::NAN)), 1.0);
    for step in [0.0, -0.01, f64::NAN] {
        assert_eq!(Seconds(0.1).one_pole_coeff_over(Seconds(step)), 0.0);
    }
    // 1000 s at 48 kHz: about 2.08e-8, not cancelled to 0.
    let long = Seconds(1_000.0).one_pole_coeff(sr);
    assert!((f64::from(long) * 48e6 - 1.0).abs() < 1e-6, "{long}");
    // Every finite positive input lands in [0, 1].
    for tau in [1e-9, 1e-3, 1.0, 1e6] {
        let c = Seconds(tau).one_pole_coeff(sr);
        assert!((0.0..=1.0).contains(&c), "τ {tau}: {c}");
    }
}

#[test]
fn seconds_to_samples_takes_the_typed_rate() {
    // The converter the DSP nodes call: it takes the typed rate, so an
    // untyped rate in a node field has to justify itself at the call.
    assert_eq!(Seconds(1.0).to_samples(SampleRate::SR_48K), Samples(48_000));
    // Raw floats still infer through `impl Into`.
    assert_eq!(
        Seconds(1.0).to_samples(SampleRate::new(48_000.0)),
        Samples(48_000)
    );
    // And the rounding split holds through the typed form.
    assert_eq!(Seconds(0.5).to_samples_floor(SampleRate(3.0)), Samples(1));
    assert_eq!(Seconds(0.5).to_samples_ceil(SampleRate(3.0)), Samples(2));
}

#[test]
fn nyquist_crosses_from_clock_rate_to_signal_frequency() {
    assert_eq!(SampleRate::SR_48K.nyquist(), Hz(24_000.0));
    assert_eq!(SampleRate::SR_44K1.nyquist(), Hz(22_050.0));

    // The margin is a fraction of *Nyquist*, not of the sample rate. The
    // call sites this replaced scaled the rate (`sr * 0.499`), so porting
    // one means doubling its constant — 0.499 of the rate is 0.998 of
    // Nyquist. Getting that backwards halves every filter's usable range,
    // so it is pinned here.
    assert_eq!(SampleRate::SR_48K.nyquist_scaled(1.0), Hz(24_000.0));
    assert_eq!(SampleRate::SR_48K.nyquist_scaled(0.5), Hz(12_000.0));
    let svf = SampleRate::SR_48K.nyquist_scaled(0.998);
    assert!((svf.get() - 48_000.0 * 0.499).abs() < 1e-3);
}

#[test]
fn nyquist_scaled_refuses_to_exceed_nyquist() {
    // A margin above unity would hand back a frequency above Nyquist —
    // the exact aliasing bug the clamp exists to stop.
    assert_eq!(SampleRate::SR_48K.nyquist_scaled(1.5), Hz(24_000.0));
    assert_eq!(SampleRate::SR_48K.nyquist_scaled(-0.2), Hz(0.0));
}

/// A `Seconds` names every frame: a frame count round-trips exactly through
/// seconds and back, well past the 256 s where an `f32` second (spacing 2^-15 s
/// at that point) stopped being able to, and out to a day at 192 kHz.
///
/// Mutation (run): round-trip through `f32` seconds in `Samples::to_seconds`
/// (`as f32 as f64`) -> the frames past 256 s at 48 kHz no longer come back ->
/// fails.
#[test]
fn a_seconds_round_trip_is_exact_for_every_frame_a_project_holds() {
    for rate in [
        SampleRate::SR_48K,
        SampleRate(96_000.0),
        SampleRate(192_000.0),
    ] {
        let round_trips = |n: usize| Samples(n).to_seconds(rate).to_samples(rate) == Samples(n);
        let day = (86_400.0 * rate.get()) as usize;
        for n in (0..=day).step_by(9_973) {
            assert!(round_trips(n), "{n} frames at {rate:?}");
        }
        // The frames around 256 s at 48 kHz, where an `f32` first failed.
        for n in 12_288_000..12_288_400 {
            assert!(round_trips(n), "{n} frames at {rate:?}");
        }
    }
}
