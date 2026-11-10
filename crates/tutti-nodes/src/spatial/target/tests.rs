use super::*;

/// A bearing crossing the ±180 seam must take the short arc. Asserted as a
/// direction of travel — the exact per-step position is the smoother's.
#[test]
fn bearing_crosses_the_seam_the_short_way() {
    let mut s = AngleSmoother::new(SampleRate(48_000.0), Samples(1));
    // Settle at 170 degrees.
    for _ in 0..48_000 {
        s.step(Azimuth(170.0), Elevation::LEVEL);
    }
    let (start, _) = s.step(Azimuth(170.0), Elevation::LEVEL);
    assert!(
        (start.get() - 170.0).abs() < 1.0,
        "should have settled near 170, got {}",
        start.get()
    );

    // Now aim at -170: 20 degrees away across the seam, NOT 340 back
    // through zero. One step must move the bearing UP past 180 (which
    // wraps to a negative value) rather than down toward 160.
    let (next, _) = s.step(Azimuth(-170.0), Elevation::LEVEL);
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
        s.step(Azimuth::FRONT, Elevation(90.0));
    }
    let (_, el) = s.step(Azimuth::FRONT, Elevation(90.0));
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
        s.step(Azimuth(45.0), Elevation(30.0));
    }
    let (az, el) = s.step(Azimuth(45.0), Elevation(30.0));
    assert!((az.get() - 45.0).abs() < 0.5, "azimuth {}", az.get());
    assert!((el.get() - 30.0).abs() < 0.5, "elevation {}", el.get());
}
