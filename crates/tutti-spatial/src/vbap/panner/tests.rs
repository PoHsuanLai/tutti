use super::*;
use tutti_types::Seconds;

/// `solve_block` lands on the exact one-pole response: after `N`
/// frames the bearing and height are `target - (target - start) *
/// (1 - coeff)^N`, with the arc taken the short way, to within 0.005
/// degrees at 48 kHz (scaled by the rate: the f32 rounding of the
/// blocks contracts over `1 / coeff` frames). The sweeps cover blocks
/// of 64 and 1024, a path across the seam and a 192 kHz rate.
///
/// Mutations (each run, each fails): `gap_closed` returning `coeff`
/// whatever the step count (one step per block: the response falls
/// 60 frames behind); `advance_angle` taking the arc as `target -
/// current` (the sweep goes the long way).
#[test]
fn solve_block_follows_the_exact_one_pole_response() {
    for (rate, block, start, target) in [
        (
            48_000.0,
            64usize,
            (20.0f32, 0.0f32),
            (Azimuth(60.0), Elevation(5.0)),
        ),
        (
            48_000.0,
            1024,
            (170.0, 0.0),
            (Azimuth(-170.0), Elevation(-30.0)),
        ),
        (
            192_000.0,
            64,
            (-170.0, 20.0),
            (Azimuth(150.0), Elevation(80.0)),
        ),
    ] {
        let tol = 0.005 * rate as f32 / 48_000.0;
        let coeff = f64::from(Seconds(0.05).one_pole_coeff(SampleRate(rate)));
        let arc = f64::from(Azimuth(start.0).shortest_arc_to(target.0).get());
        let mut panner = VbapPanner::stereo().unwrap();
        panner.set_sample_rate(SampleRate(rate));
        panner
            .smoother
            .reset_to_target(Azimuth(start.0), Elevation(start.1));
        for b in 1..=(rate as usize / block) {
            panner.solve_block(target, StereoWidth::NATURAL, Spread::POINT, block);
            let left = (1.0 - coeff).powi((b * block) as i32);
            let want = Azimuth((f64::from(start.0) + arc * (1.0 - left)) as f32);
            let want_el =
                f64::from(target.1.get()) - (f64::from(target.1.get()) - f64::from(start.1)) * left;
            let (az, el) = panner.smoother.current();
            let err = want.shortest_arc_to(az).get().abs();
            assert!(err < tol, "block {b}: bearing {az:?}, want {want:?}");
            assert!(
                (f64::from(el.get()) - want_el).abs() < f64::from(tol),
                "block {b}: height {el:?}, want {want_el}"
            );
        }
    }
}

/// A held source stops advancing and keeps its gains: after the
/// ramp settles, further blocks leave the gains and the smoother state
/// unchanged, and the gains are the VBAP solution at the target (within
/// the residual the f32 ramp leaves: under 0.001 degrees).
///
/// Mutation (run): `settled_at` set unconditionally after the first
/// advance → fails (the source is frozen early, far from the target).
#[test]
fn a_held_source_settles_on_the_vbap_solution_at_its_target() {
    let mut panner = VbapPanner::stereo().unwrap();
    let target = (Azimuth(40.0), Elevation::LEVEL);
    let mut gains = panner.solve_block(target, StereoWidth::NATURAL, Spread::POINT, 64);
    for _ in 0..2_000 {
        gains = panner.solve_block(target, StereoWidth::NATURAL, Spread::POINT, 64);
    }
    assert!(panner.settled_at.is_some());
    let state = panner.smoother.current();
    let again = panner.solve_block(target, StereoWidth::NATURAL, Spread::POINT, 64);
    assert!(same_bits(state, panner.smoother.current()));
    assert_eq!(gains, again);
    assert!((state.0.get() - 40.0).abs() < 0.001, "{state:?}");
    let want = panner
        .kernel
        .gains(target.0, target.1, StereoWidth::NATURAL, Spread::POINT);
    for s in 0..2 {
        let (g, w) = (again.pair(s), want.pair(s));
        assert!(
            (g.0 - w.0).abs() < 1e-4 && (g.1 - w.1).abs() < 1e-4,
            "{g:?} {w:?}"
        );
    }
}

/// A settled source starts moving again when its target moves.
///
/// Mutation (run): `if self.settled_at.is_none()` for the key comparison
/// (a settled source skips every later advance) → fails.
#[test]
fn a_settled_source_moves_again_when_its_target_does() {
    let mut panner = VbapPanner::stereo().unwrap();
    let a = (Azimuth(40.0), Elevation::LEVEL);
    for _ in 0..2_000 {
        panner.solve_block(a, StereoWidth::NATURAL, Spread::POINT, 64);
    }
    assert!(panner.settled_at.is_some());
    let b = (Azimuth(-60.0), Elevation(10.0));
    panner.solve_block(b, StereoWidth::NATURAL, Spread::POINT, 64);
    let (az, el) = panner.smoother.current();
    assert!(az.get() < 40.0 - 1.0, "did not leave 40: {az:?}");
    assert!(el.get() > 0.1, "height frozen: {el:?}");
    for _ in 0..4_000 {
        panner.solve_block(b, StereoWidth::NATURAL, Spread::POINT, 1024);
    }
    let (az, el) = panner.smoother.current();
    assert!((az.get() + 60.0).abs() < 0.001 && (el.get() - 10.0).abs() < 0.001);
}

/// `solve_block` for `frames` frames, and whether it left the smoother
/// where an advance that is never skipped puts it. Panics unless that
/// advance would have moved the state (else a stale skip is invisible).
fn solve_matches_an_unconditional_advance(
    panner: &mut VbapPanner,
    target: (Azimuth, Elevation),
    frames: usize,
) {
    let mut want = panner.smoother.clone();
    let before = want.current();
    let want = want.advance(target.0, target.1, frames);
    assert!(
        !same_bits(want, before),
        "the premise: this advance must move the state"
    );
    panner.solve_block(target, StereoWidth::NATURAL, Spread::POINT, frames);
    assert!(
        same_bits(panner.smoother.current(), want),
        "skipped an advance that moves: {:?} vs {want:?}",
        panner.smoother.current()
    );
}

/// Settle `panner` on `target` at `frames`-frame blocks.
fn settle(panner: &mut VbapPanner, target: (Azimuth, Elevation), frames: usize) {
    for _ in 0..20_000 {
        panner.solve_block(target, StereoWidth::NATURAL, Spread::POINT, frames);
        if panner.settled_at.is_some() {
            return;
        }
    }
    panic!("never settled");
}

/// **A settled source is released by anything that changes what an
/// advance does**: a new sample rate, a reseed, a different block length.
/// Each case settles, changes one thing, and checks the next block
/// against an advance that is never skipped.
///
/// Mutations (each run, each fails its case): dropping `frames` from the
/// `settled_at` key in `solve_block` (`frames` case: a settled 64-frame
/// source skips the 1024-frame block that still moves); dropping
/// `self.settled_at = None` from `set_sample_rate` (`rate` case: settled
/// at 192 kHz, retuned to 8 kHz, the retuned advance is skipped);
/// dropping it from `reset_state` (`reset` case: reseeded off the target,
/// the first advance back is skipped).
#[test]
fn a_settled_source_is_released_by_a_retune_a_reseed_or_a_new_block_length() {
    let target = (Azimuth(40.0), Elevation(10.0));

    // rate: the larger coefficient moves a state the smaller one held.
    let mut panner = VbapPanner::stereo().unwrap();
    panner.set_sample_rate(SampleRate(192_000.0));
    settle(&mut panner, target, 64);
    panner.set_sample_rate(SampleRate(8_000.0));
    solve_matches_an_unconditional_advance(&mut panner, target, 64);

    // reset: reseeded away from a target it had settled on.
    let mut panner = VbapPanner::stereo().unwrap();
    settle(&mut panner, target, 64);
    panner.reset_state(Azimuth(-20.0), Elevation::LEVEL);
    solve_matches_an_unconditional_advance(&mut panner, target, 64);

    // frames: stalled at 64-frame blocks, still moving at 1024.
    let mut panner = VbapPanner::stereo().unwrap();
    settle(&mut panner, target, 64);
    solve_matches_an_unconditional_advance(&mut panner, target, 1024);
}

/// A bearing across the seam takes the short way, in one big block
/// too: from 170 to -170 the value passes 180, never near 0.
///
/// Mutation (run): `advance_angle` using the plain difference
/// `target - value` → fails (the value heads to 0).
#[test]
fn a_block_across_the_seam_takes_the_short_arc() {
    let mut panner = VbapPanner::stereo().unwrap();
    panner
        .smoother
        .reset_to_target(Azimuth(170.0), Elevation(0.0));
    let target = (Azimuth(-170.0), Elevation::LEVEL);
    for _ in 0..200 {
        panner.solve_block(target, StereoWidth::NATURAL, Spread::POINT, 64);
        let az = panner.smoother.current().0.get();
        assert!(az.abs() > 169.0, "left the short arc: {az}");
    }
}
