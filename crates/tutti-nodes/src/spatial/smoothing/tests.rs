use super::*;

/// A smoother that moves the whole way in one step, so the *path* is
/// visible without waiting out an exponential ramp.
fn instant() -> ExponentialSmoother {
    let mut s = ExponentialSmoother::new(Seconds(0.05), SampleRate(48_000.0), Samples(1));
    s.smoother.set_coeff(1.0);
    s
}

#[test]
fn angular_smoothing_crosses_the_seam_instead_of_going_around() {
    let mut s = instant();
    s.seed_at(170.0);
    // 170 -> -170 is +20 degrees. The linear form would travel -340.
    assert_eq!(s.process_angle(Azimuth(-170.0)), Azimuth(-170.0));
}

#[test]
fn angular_smoothing_takes_the_short_arc_at_partial_coefficient() {
    let mut s = ExponentialSmoother::new(Seconds(0.05), SampleRate(48_000.0), Samples(1));
    s.smoother.set_coeff(0.5);
    s.seed_at(170.0);
    // Half of the +20 degree arc lands at 180, not back down near 0.
    let next = s.process_angle(Azimuth(-170.0));
    assert_eq!(next, Azimuth(-180.0));
}

#[test]
fn angular_smoothing_stays_on_the_circle_over_a_long_sweep() {
    let mut s = ExponentialSmoother::new(Seconds(0.05), SampleRate(48_000.0), Samples(1));
    s.smoother.set_coeff(0.3);
    s.seed_at(0.0);
    for _ in 0..2_000 {
        let v = s.process_angle(Azimuth(179.0));
        assert!(
            (-180.0..=180.0).contains(&v.get()),
            "bearing {v:?} left the circle"
        );
    }
}

/// `advance_angle` is the closed form of `steps` steps along the short arc:
/// from a gap `g` the remainder is `g * (1 - coeff)^steps`, for a bearing
/// across the seam, within the bound `advance` documents; zero steps move
/// nothing and an instant smoother lands on the target. (The height's own
/// closed form is `tutti_dsp::Smoother::advance`'s, pinned there.)
///
/// Mutations (each run, each fails): the retention `1 - coeff`
/// replaced by `coeff` in `gap_closed`; `advance_angle` taking the arc
/// as `target - current`.
#[test]
fn advance_is_the_closed_form_of_repeated_steps() {
    for steps in [1usize, 64, 1024, 48_000] {
        let mut s = ExponentialSmoother::new(Seconds(0.05), SampleRate(48_000.0), Samples(1));
        let coeff = f64::from(s.smoother.coeff());
        let left = (1.0 - coeff).powi(steps as i32);
        // 170 toward -170: a +20 degree arc, remainder `20 * left`.
        s.seed_at(170.0);
        let b = s.advance_angle(Azimuth(-170.0), steps).get();
        let want = Azimuth((170.0 + 20.0 * (1.0 - left)) as f32).wrap().get();
        let err = Azimuth(b).shortest_arc_to(Azimuth(want)).get().abs();
        assert!(err < 1e-3, "{steps}: {b} vs {want}");
    }
    let mut s = ExponentialSmoother::new(Seconds(0.05), SampleRate(48_000.0), Samples(1));
    s.seed_at(10.0);
    assert_eq!(s.advance_angle(Azimuth(-170.0), 0), Azimuth(10.0));
    let mut i = instant();
    i.seed_at(170.0);
    assert_eq!(i.advance_angle(Azimuth(-170.0), 5), Azimuth(-170.0));
}
