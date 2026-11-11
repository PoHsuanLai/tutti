use super::*;
use tutti_graph::contract::{drive, prepared};
use tutti_types::MaxBlock;
use tutti_types::{ChannelTopology, UnitParam};

use crate::testing::aim;

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
    let mut panner = prep(VbapPannerNode::stereo());
    aim(&panner.param_set(), Azimuth::new(0.0), Elevation::new(0.0));

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
    let mut panner = prep(VbapPannerNode::surround_5_1());
    aim(&panner.param_set(), Azimuth(45.0), Elevation(15.0));
    panner.spread.store(Spread(0.3));
    panner.width.store(StereoWidth(1.5));

    Node::reset(&mut panner);

    assert!(
        (panner.azimuth.load().get() - 45.0).abs() < 0.001,
        "reset moved the bearing to {}",
        panner.azimuth.load().get()
    );
    assert!(
        (panner.elevation.load().get() - 15.0).abs() < 0.001,
        "reset moved the height to {}",
        panner.elevation.load().get()
    );
    assert!(
        (panner.spread.load().get() - 0.3).abs() < 0.001,
        "reset changed the spread to {}",
        panner.spread.load().get()
    );
    assert!(
        (panner.width.load().get() - 1.5).abs() < 0.001,
        "reset changed the width to {}",
        panner.width.load().get()
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
/// An aim → `tick` → `reset` sequence would pass even if `reset`
/// left the smoother where it was, because the leading tick's 50 ms
/// ramp would carry it most of the way. The reset must read the
/// azimuth and elevation cells itself.
///
/// This test therefore resets with no intervening tick, which is the
/// sequence a caller writes. The `-90°` half is
/// the guard: at `+90°` a bug that seeds front-centre still leaves
/// the left channel leading, so a one-sided assertion could pass for the
/// wrong reason.
#[test]
fn reset_seats_the_smoother_on_the_commanded_position() {
    let input = [1.0f32, 1.0f32];

    for (bearing, lead, silent) in [(90.0f32, 0usize, 1usize), (-90.0, 1, 0)] {
        // Where the panner ends up once the 50 ms ramp has run out: a hard
        // pan, so the opposite channel is silent.
        let mut settled = prep(VbapPannerNode::stereo());
        aim(&settled.param_set(), Azimuth(bearing), Elevation::LEVEL);
        let mut reference = [0.0f32; 2];
        for _ in 0..48_000 {
            tick(&mut settled, &input, &mut reference);
        }
        assert!(
            reference[lead] > 0.9 && reference[silent] < 0.1,
            "the settled reference at {bearing} should be a hard pan, got {reference:?}"
        );

        // No tick between the aim and `reset`: the reset must find
        // the commanded bearing on its own.
        let mut panner = prep(VbapPannerNode::stereo());
        aim(&panner.param_set(), Azimuth(bearing), Elevation::LEVEL);
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
#[test]
fn the_de_zipper_ramp_is_in_flight_without_a_reset() {
    let input = [1.0f32, 1.0f32];

    let mut settled = prep(VbapPannerNode::stereo());
    aim(&settled.param_set(), Azimuth(90.0), Elevation::LEVEL);
    let mut reference = [0.0f32; 2];
    for _ in 0..48_000 {
        tick(&mut settled, &input, &mut reference);
    }

    let mut panner = prep(VbapPannerNode::stereo());
    aim(&panner.param_set(), Azimuth(90.0), Elevation::LEVEL);
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
    let panner = VbapPannerNode::stereo();
    aim(&panner.param_set(), Azimuth(-60.0), Elevation(20.0));

    let mut cloned = panner.clone();
    Node::reset(&mut cloned);

    assert!(
        (panner.azimuth.load().get() - (-60.0)).abs() < 0.001,
        "resetting the clone moved the original to {}",
        panner.azimuth.load().get()
    );
    assert!(
        (panner.elevation.load().get() - 20.0).abs() < 0.001,
        "resetting the clone moved the original to {}",
        panner.elevation.load().get()
    );
}

/// Every separately shared cell survives a clone as a shared handle,
/// not as a snapshot.
///
/// `Clone` hands on `azimuth`, `elevation`, `spread` and `width` by handle
/// (a `Param` clone shares its cell), so all four are asserted -- one per
/// shared field. Covering only the position leaves a hole the compiler
/// cannot see: a hand-written `Clone` building `spread` with
/// `Param::new(spread.load())` is a snapshotting clone that still passes a
/// position-only test.
///
/// Why sharing is the required behaviour rather than an implementation
/// detail: a clone is the fork source's template (`tutti_graph::param_parts`),
/// taken at insert, so a snapshotting clone would freeze every later export at
/// whatever spread or width happened to be set at insert and silently
/// ignore every later move. (The fork itself detaches; see
/// `tests/spatial_param_fork.rs`.) It is also the premise of
/// `reset_on_a_clone_does_not_move_the_original` -- `reset` must not write
/// these cells precisely because the write would reach back through every
/// handle.
///
/// Asserted in both directions per field: a handle is shared, not merely
/// copied forward, so a write through either side must be visible from the
/// other.
#[test]
fn vbap_clone_shares_atomics() {
    let panner = VbapPannerNode::stereo();
    let cloned = panner.clone();

    // -- target: azimuth + elevation, set on the original.
    aim(
        &panner.param_set(),
        Azimuth::new(90.0),
        Elevation::new(45.0),
    );
    assert!(
        (cloned.azimuth.load().get() - 90.0).abs() < 0.001,
        "the clone did not see the original's bearing: {}",
        cloned.azimuth.load().get()
    );
    assert!(
        (cloned.elevation.load().get() - 45.0).abs() < 0.001,
        "the clone did not see the original's height: {}",
        cloned.elevation.load().get()
    );

    // And vice versa.
    aim(
        &cloned.param_set(),
        Azimuth::new(-60.0),
        Elevation::new(10.0),
    );
    assert!(
        (panner.azimuth.load().get() - (-60.0)).abs() < 0.001,
        "the original did not see the clone's bearing: {}",
        panner.azimuth.load().get()
    );
    assert!(
        (panner.elevation.load().get() - 10.0).abs() < 0.001,
        "the original did not see the clone's height: {}",
        panner.elevation.load().get()
    );

    // -- spread: its own cell.
    panner.spread.store(Spread(0.3));
    assert!(
        (cloned.spread.load().get() - 0.3).abs() < 0.001,
        "the clone did not see the original's spread: {} -- a snapshotting \
         clone would freeze an offline render at the clone-time value",
        cloned.spread.load().get()
    );
    cloned.spread.store(Spread(0.8));
    assert!(
        (panner.spread.load().get() - 0.8).abs() < 0.001,
        "the original did not see the clone's spread: {}",
        panner.spread.load().get()
    );

    // -- width: its own cell too, and previously uncovered in either
    //    direction.
    panner.width.store(StereoWidth(1.5));
    assert!(
        (cloned.width.load().get() - 1.5).abs() < 0.001,
        "the clone did not see the original's width: {}",
        cloned.width.load().get()
    );
    cloned.width.store(StereoWidth(0.4));
    assert!(
        (panner.width.load().get() - 0.4).abs() < 0.001,
        "the original did not see the clone's width: {}",
        panner.width.load().get()
    );
}

/// A clone keeps the original's sample rate, so its de-zipper ramp runs at
/// the same speed: two fresh nodes at 96 kHz — the original and its clone —
/// sweep to a new bearing identically.
#[test]
fn a_clone_ramps_at_the_originals_sample_rate() {
    let rate = SampleRate(96_000.0);
    let mut original = prepared(VbapPannerNode::stereo(), rate, 64);
    let mut clone = original.clone();
    aim(&original.param_set(), Azimuth(60.0), Elevation::LEVEL);
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
#[test]
fn a_non_finite_control_never_reaches_the_smoother() {
    let bad = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY];
    for value in bad {
        for which in 0..4 {
            let mut node = prep(VbapPannerNode::surround_5_1());
            aim(&node.param_set(), Azimuth(30.0), Elevation::LEVEL);
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
                0 => node.azimuth.store(Azimuth(value)),
                1 => node.elevation.store(Elevation(value)),
                2 => node.spread.store(Spread(value)),
                _ => node.width.store(StereoWidth(value)),
            }
            // Everything else moves in the same block.
            if which != 0 {
                node.azimuth.store(Azimuth(-70.0));
            }
            if which != 2 {
                node.spread.store(Spread(0.4));
            }
            run(&mut node, 4_800);
            aim(&node.param_set(), Azimuth(10.0), Elevation(5.0));
            node.spread.store(Spread(0.1));
            node.width.store(StereoWidth(1.0));
            run(&mut node, 4_800);
            assert!(all_finite, "control {which} = {value} reached the output");
        }
    }
}

/// The `ParamSet` an insert hands back addresses the node's own cells: a move
/// through it reaches the running node, which the graph owns.
#[test]
fn the_inserted_params_move_the_running_node() {
    use tutti_graph::Solo;
    use tutti_types::Samples;

    let mut solo = Solo::new(
        VbapPannerNode::stereo(),
        Prepare::new(RATE, MaxBlock::of(Samples(64))),
    );
    aim(solo.controls(), Azimuth(90.0), Elevation::LEVEL);
    // 100 ms: past the 50 ms de-zipper.
    let ones = vec![1.0f32; 4_800];
    let out = solo.render_input(&[&ones, &ones]);
    let (l, r) = (out[0][4_799], out[1][4_799]);
    assert!(
        l > 0.9 && r < 0.1,
        "a hard-left move through the params should have landed: [{l}, {r}]"
    );
}

/// A channel's speaker is its place among the non-LFE channels of the bed's
/// topology: the LFE (index 3 of 5.1, 7.1 and 7.1.4) feeds from no speaker and
/// the channels after it take the next one, so the surrounds are not one slot
/// early. The literals are the file orders worked by hand, the bed's speakers
/// being L R C (Ls Rs) (Lrs Rrs) (TFL TFR TBL TBR) in the preset's order.
#[test]
fn a_channels_speaker_skips_the_lfe() {
    let rows: [(u16, &[Option<usize>]); 5] = [
        (2, &[Some(0), Some(1)]),
        (4, &[Some(0), Some(1), Some(2), Some(3)]),
        (6, &[Some(0), Some(1), Some(2), None, Some(3), Some(4)]),
        (
            8,
            &[
                Some(0),
                Some(1),
                Some(2),
                None,
                Some(3),
                Some(4),
                Some(5),
                Some(6),
            ],
        ),
        (
            12,
            &[
                Some(0),
                Some(1),
                Some(2),
                None,
                Some(3),
                Some(4),
                Some(5),
                Some(6),
                Some(7),
                Some(8),
                Some(9),
                Some(10),
            ],
        ),
    ];
    for (count, want) in rows {
        let topology = ChannelTopology::for_width(ChannelLayout::from(count));
        assert_eq!(speaker_of_channel(&topology), want, "{count} channels");
    }
}

/// A topology is a bed only in the engine's order: the right width in another
/// order (unassigned channels) has no angles to pan to, and the error names it.
#[test]
fn a_topology_that_is_not_a_bed_is_refused_by_name() {
    for width in [4u16, 6, 12] {
        let unassigned = ChannelTopology::unassigned(ChannelLayout::from(width));
        assert!(
            matches!(VbapPannerNode::for_topology(&unassigned), Err(VbapError::NoBed(t)) if t == unassigned),
            "{width} unassigned channels"
        );
    }
    let quad = ChannelTopology::quad();
    assert_eq!(
        VbapPannerNode::for_topology(&quad)
            .unwrap()
            .channels()
            .count() as usize,
        4
    );
}

/// The ports a lane can drive are the params the node declares, each with
/// its cell as the base the graph's modulation rides on.
#[test]
fn the_panner_declares_its_params() {
    let node = VbapPannerNode::stereo();
    node.azimuth.store(Azimuth(10.0));
    node.elevation.store(Elevation(20.0));
    node.spread.store(Spread(0.3));
    node.width.store(StereoWidth(1.5));
    assert_eq!(node.shape().params.as_slice(), &VBAP_PANNER_PARAMS[..]);
    let bases: Vec<_> = (0..5).map(|k| node.param_base(k)).collect();
    assert_eq!(
        bases,
        [Some(10.0), Some(20.0), Some(0.3), Some(1.5), None],
        "each port's base is its cell"
    );
}

/// A lane that drives a param is heard as the cell would be: a node whose
/// lane holds the value, over a cell at another, settles on the render of a
/// node whose cell holds it, for each of the four ports.
///
/// The cell and the lane are told apart by the default: a render at the
/// value differs from one at the default (else the row would pass on a node
/// that ignored both).
#[test]
fn a_lane_drives_each_param_as_its_cell_would() {
    let (left, right) = ([0.5f32; 64], [-0.25f32; 64]);
    let settle = |node: &mut VbapPannerNode, lane: Option<(usize, f32)>| {
        let held = lane.map(|(_, v)| [v; 64]);
        let mut params: [Option<&[f32]>; 4] = [None; 4];
        if let Some((k, _)) = lane {
            params[k] = held.as_ref().map(|v| &v[..]);
        }
        // Ten 50 ms time constants of blocks.
        let mut last = Vec::new();
        for _ in 0..(RATE.get() as usize / 2).div_ceil(64) {
            last = drive(node, RATE, &[&left, &right], &params);
        }
        last.iter().map(|c| c[63]).collect::<Vec<f32>>()
    };
    for (k, param, value) in [
        (0, UnitParam::Azimuth, 90.0f32),
        (1, UnitParam::Elevation, 45.0),
        (2, UnitParam::Spread, 0.8),
        (3, UnitParam::StereoWidth, 0.2),
    ] {
        let fresh = || prep(VbapPannerNode::atmos_7_1_4());
        let mut default = fresh();
        let at_default = settle(&mut default, None);

        let mut cell = fresh();
        assert!(cell.param_set().set(param, value));
        Node::reset(&mut cell);
        let at_cell = settle(&mut cell, None);
        assert!(
            at_cell
                .iter()
                .zip(&at_default)
                .any(|(a, b)| (a - b).abs() > 0.01),
            "{param:?}: the value does not change the render, so the row proves nothing"
        );

        let mut lane = fresh();
        let at_lane = settle(&mut lane, Some((k, value)));
        for (ch, (a, b)) in at_lane.iter().zip(&at_cell).enumerate() {
            assert!(
                (a - b).abs() < 1e-3,
                "{param:?}, channel {ch}: the lane settled at {a}, the cell at {b}"
            );
        }
    }
}

/// A lane-driven bearing that crosses the seam takes the short arc: 170 degrees
/// driven to 190 (which is -170) glides 20 degrees through the rear, and
/// the speakers silent at both ends (the front pair of a quad) stay silent
/// along the way. The long way round, 340 degrees through the front, lights
/// them.
///
/// The speakers that are silent at both ends are found by rendering both
/// ends settled, not named, so the row follows the bed.
#[test]
fn a_lane_driven_bearing_crosses_the_seam_the_short_way() {
    let ones = [1.0f32; 64];
    let at = |bearing: f32| {
        let mut node = prep(VbapPannerNode::quad());
        node.azimuth.store(Azimuth(bearing));
        Node::reset(&mut node);
        drive(&mut node, RATE, &[&ones[..1], &ones[..1]], &[])
            .iter()
            .map(|c| c[0])
            .collect::<Vec<f32>>()
    };
    let (start, end) = (at(170.0), at(-170.0));
    let front: Vec<usize> = (0..4)
        .filter(|&c| start[c] < 0.05 && end[c] < 0.05)
        .collect();
    assert_eq!(
        front.len(),
        2,
        "the rear pair carries both ends: {start:?} {end:?}"
    );

    let mut node = prep(VbapPannerNode::quad());
    node.azimuth.store(Azimuth(170.0));
    Node::reset(&mut node);
    let lane = [190.0f32; 64];
    let mut loudest_front = 0.0f32;
    let mut last = Vec::new();
    // Ten 50 ms time constants: the glide is over.
    for _ in 0..(RATE.get() as usize / 2).div_ceil(64) {
        last = drive(&mut node, RATE, &[&ones, &ones], &[Some(&lane)]);
        for &c in &front {
            loudest_front = last[c].iter().fold(loudest_front, |m, s| m.max(s.abs()));
        }
    }
    assert!(
        loudest_front < 0.05,
        "the glide from 170 to 190 lit the front pair ({loudest_front}): the long way round"
    );
    for c in 0..4 {
        assert!(
            (last[c][63] - end[c]).abs() < 1e-3,
            "channel {c}: settled at {}, the bearing -170 renders {}",
            last[c][63],
            end[c]
        );
    }
}
