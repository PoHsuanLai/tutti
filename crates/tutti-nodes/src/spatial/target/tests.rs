use super::*;

fn aim(azimuth: f32, elevation: f32) -> Direction {
    Direction::new(Azimuth(azimuth), Elevation(elevation))
}

/// A smoother that moves the whole way in one step, so the *path* is
/// visible without waiting out an exponential ramp.
fn with_coeff(coeff: f32) -> AngleSmoother {
    let mut s = AngleSmoother::new(SampleRate(48_000.0), Samples(1));
    s.azimuth.set_coeff(coeff);
    s.elevation.set_coeff(coeff);
    s
}

fn seeded(azimuth: f32, coeff: f32) -> AngleSmoother {
    let mut s = with_coeff(coeff);
    s.reset_to_target(aim(azimuth, 0.0));
    s
}

/// A bearing crossing the ±180 seam must take the short arc. Asserted as a
/// direction of travel — the exact per-step position is the smoother's.
#[test]
fn bearing_crosses_the_seam_the_short_way() {
    let mut s = AngleSmoother::new(SampleRate(48_000.0), Samples(1));
    // Settle at 170 degrees.
    for _ in 0..48_000 {
        s.step(aim(170.0, 0.0));
    }
    let start = s.step(aim(170.0, 0.0)).azimuth;
    assert!(
        (start.get() - 170.0).abs() < 1.0,
        "should have settled near 170, got {}",
        start.get()
    );

    // Now aim at -170: 20 degrees away across the seam, NOT 340 back
    // through zero. One step must move the bearing UP past 180 (which
    // wraps to a negative value) rather than down toward 160.
    let next = s.step(aim(-170.0, 0.0)).azimuth;
    let moved_down_through_zero = next.get() < start.get() && next.get() > 0.0;
    assert!(
        !moved_down_through_zero,
        "bearing took the long way: 170 -> {} (should cross the seam)",
        next.get()
    );
}

/// The counterpart: elevation saturates rather than wrapping.
#[test]
fn height_saturates_and_does_not_wrap() {
    let mut s = AngleSmoother::new(SampleRate(48_000.0), Samples(1));
    for _ in 0..48_000 {
        s.step(aim(0.0, 90.0));
    }
    let el = s.step(aim(0.0, 90.0)).elevation;
    assert!(
        el.get() > 80.0,
        "elevation should settle near the pole, got {}",
        el.get()
    );
}

/// Otherwise every static source sits slightly off-axis forever.
#[test]
fn settles_on_a_stationary_target() {
    let mut s = AngleSmoother::new(SampleRate(48_000.0), Samples(1));
    for _ in 0..48_000 {
        s.step(aim(45.0, 30.0));
    }
    let now = s.step(aim(45.0, 30.0));
    assert!(
        (now.azimuth.get() - 45.0).abs() < 0.5,
        "azimuth {}",
        now.azimuth.get()
    );
    assert!(
        (now.elevation.get() - 30.0).abs() < 0.5,
        "elevation {}",
        now.elevation.get()
    );
}

#[test]
fn angular_smoothing_crosses_the_seam_instead_of_going_around() {
    let mut s = seeded(170.0, 1.0);
    // 170 -> -170 is +20 degrees. The linear form would travel -340.
    assert_eq!(s.step(aim(-170.0, 0.0)).azimuth, Azimuth(-170.0));
}

#[test]
fn angular_smoothing_takes_the_short_arc_at_partial_coefficient() {
    let mut s = seeded(170.0, 0.5);
    // Half of the +20 degree arc lands at 180, not back down near 0.
    assert_eq!(s.step(aim(-170.0, 0.0)).azimuth, Azimuth(-180.0));
}

#[test]
fn angular_smoothing_stays_on_the_circle_over_a_long_sweep() {
    let mut s = seeded(0.0, 0.3);
    for _ in 0..2_000 {
        let v = s.step(aim(179.0, 0.0)).azimuth;
        assert!(
            (-180.0..=180.0).contains(&v.get()),
            "bearing {v:?} left the circle"
        );
    }
}

/// `advance` is the closed form of `steps` steps along the short arc: from a
/// gap `g` the remainder is `g * (1 - coeff)^steps`, for a bearing across the
/// seam, within the bound `advance` documents; zero steps move nothing and an
/// instant smoother lands on the target. (The height's own closed form is
/// `tutti_dsp::Smoother::advance`'s, pinned there.)
#[test]
fn advance_is_the_closed_form_of_repeated_steps() {
    for steps in [1usize, 64, 1024, 48_000] {
        let mut s = AngleSmoother::new(SampleRate(48_000.0), Samples(1));
        let coeff = f64::from(s.azimuth.coeff());
        let left = (1.0 - coeff).powi(steps as i32);
        // 170 toward -170: a +20 degree arc, remainder `20 * left`.
        s.reset_to_target(aim(170.0, 0.0));
        let b = s.advance(aim(-170.0, 0.0), steps).azimuth.get();
        let want = Azimuth((170.0 + 20.0 * (1.0 - left)) as f32).wrap().get();
        let err = Azimuth(b).shortest_arc_to(Azimuth(want)).get().abs();
        assert!(err < 1e-3, "{steps}: {b} vs {want}");
    }
    let mut s = AngleSmoother::new(SampleRate(48_000.0), Samples(1));
    s.reset_to_target(aim(10.0, 0.0));
    assert_eq!(s.advance(aim(-170.0, 0.0), 0).azimuth, Azimuth(10.0));
    let mut i = seeded(170.0, 1.0);
    assert_eq!(i.advance(aim(-170.0, 0.0), 5).azimuth, Azimuth(-170.0));
}
