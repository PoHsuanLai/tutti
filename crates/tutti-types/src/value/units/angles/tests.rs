use super::*;

/// What is deliberately *absent* is as load-bearing as what is present.
/// These must not compile; the ledger is here so a future contributor does
/// not "helpfully" add them.
///
/// - `Azimuth < Azimuth`, `Azimuth.clamp(..)` — a wrapping coordinate has
///   no ordering and no saturating clamp. `wrap` is the constraint.
/// - `Radians < Radians`, `Radians.clamp(..)` — the same, for the radian
///   coordinate; `wrapped_signed` is the constraint.
/// - `Azimuth + ArcDegrees` — `rotate_by`, which wraps; the operator would
///   not. Same for `Elevation + ArcDegrees` vs `tilt_by`, which clamps.
/// - `Azimuth - Azimuth` — `shortest_arc_to`; a plain subtraction takes the
///   long way around the seam.
/// - `Phase + Phase`, `Phase * f32`, `Phase.clamp(..)` — a cycle position.
///   Clamping pins it at 0.999 where it belongs at the next cycle's start.
/// - `Phase % Phase` — `%` is one of the two sign-preserving wraps this
///   type was introduced to eliminate. `wrapped` is the only wrap.
/// - `Phase + PhaseIncrement` — `advance`, which wraps; the operator would
///   not.
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
fn azimuth_wraps_rather_than_saturating() {
    // The shipped bug this type exists to prevent: a saturating clamp maps
    // 190 degrees to 180 (hard left). It is 170 degrees to the *right*.
    assert_eq!(Azimuth(190.0).wrap(), Azimuth(-170.0));
    assert_eq!(Azimuth(-190.0).wrap(), Azimuth(170.0));

    // `rem_euclid`, not `%`: several full turns in either direction still
    // land in range, which plain `%` does not manage for negatives.
    assert_eq!(Azimuth(360.0 + 45.0).wrap(), Azimuth(45.0));
    assert_eq!(Azimuth(-720.0 - 45.0).wrap(), Azimuth(-45.0));

    // The range is half-open at +180: one endpoint, not two names for it.
    assert_eq!(Azimuth(180.0).wrap(), Azimuth(-180.0));
}

#[test]
fn azimuth_takes_the_short_way_across_the_seam() {
    // The second shipped bug: `to - self` reports -340 here. The listener
    // hears the panner sweep almost all the way around to travel 20
    // degrees.
    let from = Azimuth(170.0);
    let to = Azimuth(-170.0);
    assert_eq!(from.shortest_arc_to(to), ArcDegrees(20.0));
    assert_eq!(to.shortest_arc_to(from), ArcDegrees(-20.0));

    // And the interpolator built on it crosses the seam rather than
    // retreating from it: halfway from 170 to -170 is 180, not 0.
    assert_eq!(from.lerp_shortest(to, 0.5), Azimuth(-180.0));
    assert_eq!(from.lerp_shortest(to, 0.0), from);
    assert_eq!(from.lerp_shortest(to, 1.0), to);
}

#[test]
fn azimuth_rotation_stays_on_the_circle() {
    assert_eq!(Azimuth(170.0).rotate_by(ArcDegrees(20.0)), Azimuth(-170.0));
    // Mirroring across the front axis — the one operator a bearing keeps.
    assert_eq!(-Azimuth(45.0), Azimuth(-45.0));
}

#[test]
fn elevation_saturates_where_azimuth_wraps() {
    // Same numbers, opposite correct answers — which is why one angle
    // type could not serve both. Past the pole, elevation stops.
    assert_eq!(Elevation::new_clamped(120.0), Elevation::UP);
    assert_eq!(Elevation::new_clamped(-120.0), Elevation::DOWN);
    // Where the identical azimuth input wraps to the far side instead.
    assert_eq!(Azimuth(120.0).wrap(), Azimuth(120.0));
    assert_eq!(Azimuth(200.0).wrap(), Azimuth(-160.0));

    assert_eq!(Elevation::LEVEL.tilt_by(ArcDegrees(30.0)), Elevation(30.0));
    assert_eq!(Elevation(80.0).tilt_by(ArcDegrees(30.0)), Elevation::UP);

    // Ordering is meaningful here and absent on `Azimuth`.
    assert!(Elevation::DOWN < Elevation::LEVEL);
    assert!(Elevation::LEVEL < Elevation::UP);
}

#[test]
fn elevation_lerp_needs_no_seam_handling() {
    assert_eq!(Elevation(0.0).lerp(Elevation(90.0), 0.5), Elevation(45.0));
    assert_eq!(
        Elevation(-90.0).lerp(Elevation(90.0), 0.5),
        Elevation::LEVEL
    );
}

#[test]
fn phase_wrap_lands_a_negative_phase_in_range() {
    // `%` and `fract` keep the sign of the input, so a negative phase would
    // stay negative and index off the front of a shape table.
    assert_eq!(-0.25_f32 % 1.0, -0.25);
    assert_eq!((-0.25_f32).fract(), -0.25);
    // `rem_euclid` — the only wrap `Phase` performs.
    assert_eq!(Phase::wrapped(-0.25), Phase(0.75));

    assert_eq!(Phase::wrapped(1.25), Phase(0.25));
    assert_eq!(Phase::wrapped(0.5), Phase(0.5));
    // Many turns out, either direction, still lands in range.
    assert_eq!(Phase::wrapped(-3.25), Phase(0.75));
    assert_eq!(Phase::wrapped(7.5), Phase(0.5));
}

/// `wrapped`'s two shortcuts are `rem_euclid`, bit for bit — including at
/// the edges of each range, where a sloppier wrap (`v - 1.0` for all
/// `v >= 1`, or `< 1.0` tested as `<= 1.0`) would hand back `1.0`, which is
/// not a phase.
#[test]
fn the_wrap_shortcuts_are_rem_euclid_exactly() {
    let mut probes = vec![
        0.0f32,
        -0.0,
        1.0,
        2.0,
        -1.0,
        f32::MIN_POSITIVE,
        1.0 - f32::EPSILON / 2.0,
        1.0 + f32::EPSILON,
        2.0 - f32::EPSILON,
        f32::NAN,
        f32::INFINITY,
    ];
    let mut state = 0x1234_5678_u32;
    for _ in 0..20_000 {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        probes.push((state >> 8) as f32 / (1u32 << 24) as f32 * 6.0 - 2.0);
    }
    for v in probes {
        let want = v.rem_euclid(1.0);
        let got = Phase::wrapped(v).get();
        assert!(
            got.to_bits() == want.to_bits() || (got.is_nan() && want.is_nan()),
            "wrapped({v:e}) = {got:e}, rem_euclid gives {want:e}"
        );
    }
}

#[test]
fn phase_advances_backwards_without_escaping_the_cycle() {
    // A reverse-rate modulator: the case a sign-preserving wrap breaks.
    let p = Phase(0.1).advance(PhaseIncrement(-0.25));
    assert_eq!(p, Phase(0.85));
    assert!((0.0..1.0).contains(&p.get()));

    // Forward across the seam.
    assert_eq!(
        Phase(0.9).advance(PhaseIncrement(0.25)),
        Phase::wrapped(1.15)
    );

    // Walking a full cycle backwards stays in range at every step.
    let mut cursor = Phase::START;
    for _ in 0..40 {
        cursor = cursor.advance(PhaseIncrement(-0.1));
        assert!((0.0..1.0).contains(&cursor.get()));
    }
}

#[test]
fn phase_increment_derives_from_frequency_and_rate() {
    // One cycle per second at 100 Hz sampling = 1/100 turn per sample.
    assert_eq!(
        PhaseIncrement::per_sample(Hz(1.0), SampleRate::new(100.0)),
        PhaseIncrement(0.01)
    );
    // A degenerate rate yields a standing phase rather than inf/NaN.
    assert_eq!(
        PhaseIncrement::per_sample(Hz(440.0), SampleRate::new(0.0)),
        PhaseIncrement(0.0)
    );
}

/// `wrapped_signed` holds `[-pi, pi)` exactly for every finite input, with
/// `pi` the `f32` constant, and is the identity inside it.
///
/// The figures are `x mod 2pi` taken from a 80-digit evaluation, not from
/// the code under test. Up to `2^24` the result is within one `f32` ulp of
/// it; beyond that only the range is asserted, since the `f64` `2pi` no
/// longer holds the turn count.
#[test]
fn wrapped_signed_holds_its_range_exactly() {
    use core::f32::consts::PI;
    let in_range = |w: f32| (-PI..PI).contains(&w);
    let near = |w: f32, want: f64| (f64::from(w) - want).abs() < 3.0e-7;

    // Identity inside, bits and all: both ends of the closed side, the
    // largest value under `pi`, the zeros and the subnormals.
    for x in [
        -PI,
        PI.next_down(),
        0.0,
        -0.0,
        1.0,
        -1.0,
        f32::MIN_POSITIVE,
        -f32::MIN_POSITIVE,
        f32::from_bits(1),
        -f32::from_bits(1),
        f32::from_bits(0x007f_ffff),
    ] {
        assert_eq!(Radians(x).wrapped_signed().get().to_bits(), x.to_bits());
    }

    // `+pi` is the excluded end and folds to `-pi`.
    assert_eq!(Radians(PI).wrapped_signed(), Radians(-PI));

    // `x mod 2pi` into `[-pi, pi)`, exact to 80 digits.
    for (x, want) in [
        (3.0 * PI, -3.141_592_629_740_032_3),
        (16_384.0, -2.547_281_124_361_531_6),
        (33_554_432.0, -1.787_937_733_360_393_9),
        (1.0e6, -0.357_564_167_085_735),
        (-1.0e6, 0.357_564_167_085_735),
    ] {
        let w = Radians(x).wrapped_signed().get();
        assert!(in_range(w) && near(w, want), "{x:e} -> {w:e}, want {want}");
    }
    // The value just under `pi` that rounds to `pi` is the excluded end.
    assert_eq!(Radians(-3.0 * PI).wrapped_signed(), Radians(-PI));

    // Every magnitude, up to the largest finite `f32` and down to the
    // subnormals, in both signs.
    for bits in (1..0x7f80_0000_u32).step_by(4099) {
        let x = f32::from_bits(bits);
        for v in [x, -x, x.next_up(), x.next_down()] {
            let w = Radians(v).wrapped_signed().get();
            assert!(in_range(w), "{v:e} -> {w:e}");
        }
    }
    for v in [f32::MAX, -f32::MAX, f32::MAX.next_down(), f32::MIN] {
        let w = Radians(v).wrapped_signed().get();
        assert!(in_range(w), "{v:e} -> {w:e}");
    }
}

/// `Radians` wraps through `wrapped_signed`, never a saturating clamp.
#[test]
fn radians_wrap_signed_rather_than_saturating() {
    let pi = core::f32::consts::PI;

    // A deviation of +3pi/2 is really -pi/2. Keeping the larger number
    // moves a partial the wrong way round the circle.
    assert!((Radians(1.5 * pi).wrapped_signed().get() - (-0.5 * pi)).abs() < 1e-5);
    assert!((Radians(-1.5 * pi).wrapped_signed().get() - (0.5 * pi)).abs() < 1e-5);

    // Already inside: unchanged.
    assert_eq!(Radians(0.0).wrapped_signed(), Radians(0.0));

    // Half-open at +pi, which folds to -pi — the same point on the circle,
    // and the same convention `Phase::wrapped` uses at the top of a cycle.
    assert!((Radians(pi).wrapped_signed().get() - (-pi)).abs() < 1e-5);

    // Arithmetic, not iteration: a phase accumulated over minutes of audio
    // wraps in constant time. The `while` loop this replaced ran once per
    // turn, on the audio thread.
    let far = Radians(1.0e6).wrapped_signed().get();
    assert!((-pi..pi).contains(&far), "{far}");
}

#[test]
fn turns_and_radians_convert_both_ways() {
    assert_eq!(Phase(0.5).to_radians(), Radians(core::f32::consts::PI));
    assert_eq!(Phase::START.to_radians(), Radians(0.0));
    assert_eq!(Radians::TAU.to_phase(), Phase::START);
    assert_eq!(Radians(core::f32::consts::PI).to_phase(), Phase(0.5));

    // A quarter turn is the sine peak — the check that the units line up
    // rather than being off by tau somewhere.
    assert!((Phase(0.25).to_radians().sin() - 1.0).abs() < 1e-6);
    assert!(Phase::START.to_radians().cos() - 1.0 < 1e-6);

    // Degrees reach radians too, so a panner converts once instead of
    // hand-multiplying by pi/180 at each trig call.
    assert!((Radians::from(Azimuth(180.0)).0 - core::f32::consts::PI).abs() < 1e-6);
    assert_eq!(Radians::from(Elevation::LEVEL), Radians(0.0));
}

#[test]
fn arc_degrees_composes_like_the_displacement_it_is() {
    assert_eq!(ArcDegrees(20.0) + ArcDegrees(15.0), ArcDegrees(35.0));
    assert_eq!(ArcDegrees(20.0) * 0.5, ArcDegrees(10.0));
    assert_eq!(-ArcDegrees(20.0), ArcDegrees(-20.0));
    assert!(ArcDegrees(10.0) < ArcDegrees(20.0));
}
