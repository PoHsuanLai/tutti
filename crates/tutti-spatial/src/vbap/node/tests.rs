use super::*;
use core::sync::atomic::Ordering;
use tutti_graph::contract::{drive, prepared};

const RATE: SampleRate = SampleRate(48_000.0);

/// `node`, prepared at 48 kHz for blocks of up to 64 frames.
fn prep(node: VbapPannerNode) -> VbapPannerNode {
    prepared(node, RATE, 64)
}

/// One block of one frame, written to `out`.
fn tick(node: &mut VbapPannerNode, input: &[f32; 2], out: &mut [f32]) {
    tick_at(node, RATE, input, out);
}

fn tick_at(node: &mut VbapPannerNode, rate: SampleRate, input: &[f32; 2], out: &mut [f32]) {
    let rendered = drive(node, rate, &[&input[..1], &input[1..]], &[]);
    for (o, c) in out.iter_mut().zip(rendered) {
        *o = c[0];
    }
}

#[test]
fn vbap_panner_tick() {
    let mut panner = prep(VbapPannerNode::stereo().unwrap());
    panner
        .controls()
        .set_position(Azimuth::new(0.0), Elevation::new(0.0));

    let input = [1.0f32, 1.0f32];
    let mut output = [0.0f32; 2];

    tick(&mut panner, &input, &mut output);

    assert!(output[0] > 0.0);
    assert!(output[1] > 0.0);
}

/// `Node::reset` resets time, not settings.
///
/// A fork (an offline export's copy of the live graph) is reset before it
/// renders, to drop inherited state; a host resets between clips for the
/// same reason. A reset that re-aimed would move every spatialised
/// source to front-centre in both cases, with nothing to compare and no
/// error raised.
#[test]
fn reset_keeps_the_authored_placement() {
    let mut panner = prep(VbapPannerNode::surround_5_1().unwrap());
    panner
        .controls()
        .set_position(Azimuth(45.0), Elevation(15.0));
    panner.controls().set_spread(Spread(0.3));
    panner.controls().set_width(StereoWidth(1.5));

    Node::reset(&mut panner);

    assert!(
        (panner.controls().azimuth().get() - 45.0).abs() < 0.001,
        "reset moved the bearing to {}",
        panner.controls().azimuth().get()
    );
    assert!(
        (panner.controls().elevation().get() - 15.0).abs() < 0.001,
        "reset moved the height to {}",
        panner.controls().elevation().get()
    );
    assert!(
        (panner.controls().spread().get() - 0.3).abs() < 0.001,
        "reset changed the spread to {}",
        panner.controls().spread().get()
    );
    assert!(
        (panner.controls().width().get() - 1.5).abs() < 0.001,
        "reset changed the width to {}",
        panner.controls().width().get()
    );
}

/// The other half of the contract: the ramp *is* cleared, so the frame
/// after a reset renders at the commanded position rather than sweeping in
/// from wherever the smoother had got to.
///
/// Asserted against a *settled* reference panner rather than against a
/// channel inequality: the de-zipper starts at front-centre, so a few frames
/// into a hard-left move both channels are still near-equal and either one
/// may lead by a hair. "Equals the settled answer" is the property a seated
/// smoother actually has, and it is the one that fails when the ramp is
/// left in flight.
///
/// # The setup deliberately does not tick before the reset
///
/// A `set_position` → `tick` → `reset` sequence would pass even if `reset`
/// left the smoother where it was, because the leading tick's 50 ms
/// ramp would carry it most of the way. The reset must read the
/// [`SpatialTarget`] itself.
///
/// This test therefore resets with no intervening tick, which is the
/// sequence a caller writes. The `-90°` half is
/// the mutation guard: at `+90°` a bug that seeds front-centre still leaves
/// the left channel leading, so a one-sided assertion could pass for the
/// wrong reason.
///
/// Mutation: seat `VbapPannerNode::reset` on `Azimuth::FRONT` instead of
/// the cells' bearing → both halves fail, reading `[0.707, 0.707]`
/// (front-centre) instead of a hard pan.
#[test]
fn reset_seats_the_smoother_on_the_commanded_position() {
    let input = [1.0f32, 1.0f32];

    for (bearing, lead, silent) in [(90.0f32, 0usize, 1usize), (-90.0, 1, 0)] {
        // Where the panner ends up once the 50 ms ramp has run out: a hard
        // pan, so the opposite channel is silent.
        let mut settled = prep(VbapPannerNode::stereo().unwrap());
        settled
            .controls()
            .set_position(Azimuth(bearing), Elevation::LEVEL);
        let mut reference = [0.0f32; 2];
        for _ in 0..48_000 {
            tick(&mut settled, &input, &mut reference);
        }
        assert!(
            reference[lead] > 0.9 && reference[silent] < 0.1,
            "the settled reference at {bearing} should be a hard pan, got {reference:?}"
        );

        // No tick between `set_position` and `reset`: the reset must find
        // the commanded bearing on its own.
        let mut panner = prep(VbapPannerNode::stereo().unwrap());
        panner
            .controls()
            .set_position(Azimuth(bearing), Elevation::LEVEL);
        Node::reset(&mut panner);

        let mut after = [0.0f32; 2];
        tick(&mut panner, &input, &mut after);
        assert!(
            (after[0] - reference[0]).abs() < 0.01 && (after[1] - reference[1]).abs() < 0.01,
            "at {bearing}deg, the first frame after a reset should already render \
             at the commanded position: got {after:?}, settled is {reference:?}"
        );
    }
}

/// The ramp is genuinely in flight before a reset — otherwise
/// [`reset_seats_the_smoother_on_the_commanded_position`] would hold
/// trivially and prove nothing about the reset.
///
/// Mutation: make `AngleSmoother::step` jump straight to the target (coeff
/// 1.0) → fails, because the first frame would already be settled.
#[test]
fn the_de_zipper_ramp_is_in_flight_without_a_reset() {
    let input = [1.0f32, 1.0f32];

    let mut settled = prep(VbapPannerNode::stereo().unwrap());
    settled
        .controls()
        .set_position(Azimuth(90.0), Elevation::LEVEL);
    let mut reference = [0.0f32; 2];
    for _ in 0..48_000 {
        tick(&mut settled, &input, &mut reference);
    }

    let mut panner = prep(VbapPannerNode::stereo().unwrap());
    panner
        .controls()
        .set_position(Azimuth(90.0), Elevation::LEVEL);
    let mut mid_ramp = [0.0f32; 2];
    tick(&mut panner, &input, &mut mid_ramp);
    assert!(
        (mid_ramp[1] - reference[1]).abs() > 0.1,
        "one frame in, the ramp should still be far from settled, \
         got {mid_ramp:?} vs {reference:?}"
    );
}

/// `Clone` shares the position atomics, so a reset that wrote them would
/// reach back through every handle — including the live node an offline
/// render was cloned from.
#[test]
fn reset_on_a_clone_does_not_move_the_original() {
    let panner = VbapPannerNode::stereo().unwrap();
    panner
        .controls()
        .set_position(Azimuth(-60.0), Elevation(20.0));

    let mut cloned = panner.clone();
    Node::reset(&mut cloned);

    assert!(
        (panner.controls().azimuth().get() - (-60.0)).abs() < 0.001,
        "resetting the clone moved the original to {}",
        panner.controls().azimuth().get()
    );
    assert!(
        (panner.controls().elevation().get() - 20.0).abs() < 0.001,
        "resetting the clone moved the original to {}",
        panner.controls().elevation().get()
    );
}

/// Every separately shared cell survives a clone as a shared handle,
/// not as a snapshot.
///
/// `Clone` hands on `target`, `spread` and `width` by handle (a `Param`
/// clone shares its cell), so all three are asserted -- one per shared
/// field. Covering only `target` leaves a hole the compiler cannot see: a
/// hand-written `VbapPannerControls::clone` building `spread` with
/// `Param::new(spread.load())` is a snapshotting clone that still passes a
/// position-only test.
///
/// Why sharing is the required behaviour rather than an implementation
/// detail: a clone is the fork source's template (`crate::fork`), taken at
/// insert, so a snapshotting clone would freeze every later export at
/// whatever spread or width happened to be set at insert and silently
/// ignore every later move. (The fork itself detaches; see
/// `tests/isolate_snapshots.rs`.) It is also the premise of
/// `reset_on_a_clone_does_not_move_the_original` -- `reset` must not write
/// these cells precisely because the write would reach back through every
/// handle.
///
/// Asserted in both directions per field: a handle is shared, not merely
/// copied forward, so a write through either side must be visible from the
/// other.
#[test]
fn vbap_clone_shares_atomics() {
    let panner = VbapPannerNode::stereo().unwrap();
    let cloned = panner.clone();

    // -- target: azimuth + elevation, set on the original.
    panner
        .controls()
        .set_position(Azimuth::new(90.0), Elevation::new(45.0));
    assert!(
        (cloned.controls().azimuth().get() - 90.0).abs() < 0.001,
        "the clone did not see the original's bearing: {}",
        cloned.controls().azimuth().get()
    );
    assert!(
        (cloned.controls().elevation().get() - 45.0).abs() < 0.001,
        "the clone did not see the original's height: {}",
        cloned.controls().elevation().get()
    );

    // And vice versa.
    cloned
        .controls()
        .set_position(Azimuth::new(-60.0), Elevation::new(10.0));
    assert!(
        (panner.controls().azimuth().get() - (-60.0)).abs() < 0.001,
        "the original did not see the clone's bearing: {}",
        panner.controls().azimuth().get()
    );
    assert!(
        (panner.controls().elevation().get() - 10.0).abs() < 0.001,
        "the original did not see the clone's height: {}",
        panner.controls().elevation().get()
    );

    // -- spread: its own cell.
    panner.controls().set_spread(Spread(0.3));
    assert!(
        (cloned.controls().spread().get() - 0.3).abs() < 0.001,
        "the clone did not see the original's spread: {} -- a snapshotting \
         clone would freeze an offline render at the clone-time value",
        cloned.controls().spread().get()
    );
    cloned.controls().set_spread(Spread(0.8));
    assert!(
        (panner.controls().spread().get() - 0.8).abs() < 0.001,
        "the original did not see the clone's spread: {}",
        panner.controls().spread().get()
    );

    // -- width: its own cell too, and previously uncovered in either
    //    direction.
    panner.controls().set_width(StereoWidth(1.5));
    assert!(
        (cloned.controls().width().get() - 1.5).abs() < 0.001,
        "the clone did not see the original's width: {}",
        cloned.controls().width().get()
    );
    cloned.controls().set_width(StereoWidth(0.4));
    assert!(
        (panner.controls().width().get() - 0.4).abs() < 0.001,
        "the original did not see the clone's width: {}",
        panner.controls().width().get()
    );
}

/// A clone keeps the original's sample rate, so its de-zipper ramp runs at
/// the same speed: two fresh nodes at 96 kHz — the original and its clone —
/// sweep to a new bearing identically.
///
/// Mutation (run): dropping `new_panner.set_sample_rate(self.sample_rate)`
/// from `Clone` leaves the clone's smoother at 48 kHz (the ramp twice as
/// fast) and fails.
#[test]
fn a_clone_ramps_at_the_originals_sample_rate() {
    let rate = SampleRate(96_000.0);
    let mut original = prepared(VbapPannerNode::stereo().unwrap(), rate, 64);
    let mut clone = original.clone();
    original
        .controls()
        .set_position(Azimuth(60.0), Elevation::LEVEL);
    let (mut a, mut b) = ([0.0f32; 2], [0.0f32; 2]);
    for i in 0..2_000 {
        tick_at(&mut original, rate, &[1.0, 1.0], &mut a);
        tick_at(&mut clone, rate, &[1.0, 1.0], &mut b);
        assert_eq!(a, b, "frame {i}: the clone's ramp diverged");
    }
}

/// A non-finite position, spread or width never reaches the de-zipper
/// smoother, whose state is recursive: the node keeps the last finite
/// value, renders finite audio, and follows the next finite write.
///
/// Mutation (each run, each fails): reading the azimuth, elevation, spread
/// or width raw (no `hold_finite`) — the first two leave the smoother, and
/// so every later frame, NaN.
#[test]
fn a_non_finite_control_never_reaches_the_smoother() {
    let bad = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY];
    for value in bad {
        for which in 0..4 {
            let mut node = prep(VbapPannerNode::surround_5_1().unwrap());
            node.controls()
                .set_position(Azimuth(30.0), Elevation::LEVEL);
            let (mut out, input) = ([0.0f32; 6], [0.5f32, -0.25]);
            let mut all_finite = true;
            let mut run = |node: &mut VbapPannerNode, frames: usize| {
                for _ in 0..frames {
                    tick(node, &input, &mut out);
                    all_finite &= out.iter().all(|s| s.is_finite());
                }
            };
            run(&mut node, 64);
            match which {
                0 => node
                    .controls
                    .target
                    .azimuth
                    .as_atomic()
                    .store(value, Ordering::Release),
                1 => node
                    .controls
                    .target
                    .elevation
                    .as_atomic()
                    .store(value, Ordering::Release),
                2 => node
                    .controls
                    .spread
                    .as_atomic()
                    .store(value, Ordering::Release),
                _ => node
                    .controls
                    .width
                    .as_atomic()
                    .store(value, Ordering::Release),
            }
            // Everything else moves in the same block.
            if which != 0 {
                node.controls
                    .target
                    .azimuth
                    .as_atomic()
                    .store(-70.0, Ordering::Release);
            }
            if which != 2 {
                node.controls
                    .spread
                    .as_atomic()
                    .store(0.4, Ordering::Release);
            }
            run(&mut node, 4_800);
            node.controls().set_position(Azimuth(10.0), Elevation(5.0));
            node.controls().set_spread(Spread(0.1));
            node.controls().set_width(StereoWidth(1.0));
            run(&mut node, 4_800);
            assert!(all_finite, "control {which} = {value} reached the output");
        }
    }
}

/// The controls an insert hands back are the node's own cells: a move
/// through them reaches the running node, which the graph owns.
///
/// Mutation (run): `into_parts` hands back `VbapPannerControls::new()`
/// (cells of its own) → the source stays front-centre, `[0.707, 0.707]`
/// → fails.
#[test]
fn the_inserted_controls_move_the_running_node() {
    use tutti_graph::Solo;
    use tutti_types::Samples;

    let mut solo = Solo::new(
        VbapPannerNode::stereo().unwrap(),
        Prepare::new(RATE, Samples(64)),
    );
    solo.controls()
        .set_position(Azimuth(90.0), Elevation::LEVEL);
    // 100 ms: past the 50 ms de-zipper.
    let ones = vec![1.0f32; 4_800];
    let out = solo.render_input(&[&ones, &ones]);
    let (l, r) = (out[0][4_799], out[1][4_799]);
    assert!(
        l > 0.9 && r < 0.1,
        "a hard-left move through the controls should have landed: [{l}, {r}]"
    );
}
