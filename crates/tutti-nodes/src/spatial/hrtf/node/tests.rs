use super::*;
use crate::testing::aim;
use tutti_dsp::HRTF_STEP;
use tutti_graph::contract::{drive, prepared};
use tutti_types::ChannelLayout;
use tutti_types::MaxBlock;

const RATE: SampleRate = SampleRate(44_100.0);
const BLOCK: usize = 64;

/// One block of one frame.
fn tick(node: &mut HrtfBinauralNode, input: [f32; 2]) -> [f32; 2] {
    let out = drive(node, RATE, &[&input[..1], &input[1..]], &[]);
    [out[0][0], out[1][0]]
}

/// Serialize a minimal but format-valid HRIR sphere: a tetrahedron of 4
/// vertices with unit-impulse HRIRs (left = identity delta, right scaled).
/// Enough to parse, triangulate, and convolve — no dataset file needed to
/// exercise the renderer.
fn synthetic_hrir_sphere(sample_rate: u32, ir_len: usize) -> Vec<u8> {
    fn push_u32(b: &mut Vec<u8>, v: u32) {
        b.extend_from_slice(&v.to_le_bytes());
    }
    fn push_f32(b: &mut Vec<u8>, v: f32) {
        b.extend_from_slice(&v.to_le_bytes());
    }

    let verts: [[f32; 3]; 4] = [
        [1.0, 1.0, 1.0],
        [-1.0, -1.0, 1.0],
        [-1.0, 1.0, -1.0],
        [1.0, -1.0, -1.0],
    ];
    let faces: [[u32; 3]; 4] = [[0, 1, 2], [0, 3, 1], [0, 2, 3], [1, 3, 2]];

    let mut b = Vec::new();
    b.extend_from_slice(b"HRIR");
    push_u32(&mut b, sample_rate);
    push_u32(&mut b, ir_len as u32);
    push_u32(&mut b, verts.len() as u32);
    push_u32(&mut b, (faces.len() * 3) as u32);
    for f in faces {
        for idx in f {
            push_u32(&mut b, idx);
        }
    }
    for (vi, v) in verts.iter().enumerate() {
        push_f32(&mut b, v[0]);
        push_f32(&mut b, v[1]);
        push_f32(&mut b, v[2]);
        // Left HRIR: unit delta at t=0.
        for i in 0..ir_len {
            push_f32(&mut b, if i == 0 { 1.0 } else { 0.0 });
        }
        // Right HRIR: delta scaled per-vertex so L and R differ.
        for i in 0..ir_len {
            push_f32(&mut b, if i == 0 { 0.5 + 0.1 * vi as f32 } else { 0.0 });
        }
    }
    b
}

fn make_node() -> HrtfBinauralNode {
    let bytes = synthetic_hrir_sphere(44_100, 64);
    let node = HrtfBinauralNode::new(&bytes, RATE).expect("synthetic sphere should parse");
    prepared(node, RATE, BLOCK)
}

/// A NaN or infinite position is ignored rather than sticking: while it
/// is set the de-zipper stays aimed at the last finite position, and a
/// finite position after it is reached as usual.
///
/// Asserted on the de-zipper's aim, the direction each step renders:
/// the output alone cannot show it, since the sphere's face search
/// answers some face for a NaN direction and the output stays finite,
/// at the wrong place. Without `LastGood` the smoother's state turns
/// NaN on the first step and stays NaN through every later target.
#[test]
fn a_non_finite_position_is_ignored_and_does_not_stick() {
    let mut node = make_node();
    let block = |node: &mut HrtfBinauralNode| {
        let input: Vec<f32> = (0..BLOCK).map(|i| ((i as f32) * 0.05).sin()).collect();
        drive(node, RATE, &[&input, &input], &[])
    };
    // 0.5 s: ten 50 ms time constants, so the aim settles to well
    // within the tolerance below.
    let settle = |node: &mut HrtfBinauralNode| {
        for _ in 0..(RATE.get() as usize / 2).div_ceil(BLOCK) {
            let out = block(node);
            assert!(out.iter().flatten().all(|s| s.is_finite()));
        }
    };
    let near = |aim: Direction, want: (f32, f32)| {
        (aim.azimuth.get() - want.0).abs() < 0.01 && (aim.elevation.get() - want.1).abs() < 0.01
    };
    aim(&node.param_set(), Azimuth::new(-45.0), Elevation::new(30.0));
    settle(&mut node);
    assert!(
        near(node.aim.current(), (-45.0, 30.0)),
        "{:?}",
        node.aim.current()
    );
    // An infinite bearing wraps to NaN on the way into the cell; an
    // infinite height is clamped to the pole there, so it is a position.
    for (az, el) in [(f32::NAN, 0.0), (0.0, f32::NAN), (f32::INFINITY, 10.0)] {
        // Only the bad coordinate is ignored; the other moves.
        let want = (
            if az.is_finite() { az } else { -45.0 },
            if el.is_finite() { el } else { 30.0 },
        );
        aim(&node.param_set(), Azimuth::new(az), Elevation::new(el));
        settle(&mut node);
        assert!(
            near(node.aim.current(), want),
            "({az}, {el}): aimed at {:?}, want {want:?}",
            node.aim.current()
        );
        aim(&node.param_set(), Azimuth::new(-45.0), Elevation::new(30.0));
        settle(&mut node);
        assert!(
            near(node.aim.current(), (-45.0, 30.0)),
            "did not recover from ({az}, {el}): {:?}",
            node.aim.current()
        );
    }
    // The cells hold any `f32` raw (a param write is not normalized), so ±∞
    // reaches the node as it wrote it; `LastGood` must refuse it there.
    for (az, el) in [
        (f32::INFINITY, 30.0),
        (f32::NEG_INFINITY, 30.0),
        (-45.0, f32::INFINITY),
        (-45.0, f32::NEG_INFINITY),
    ] {
        node.azimuth.store(Azimuth(az));
        node.elevation.store(Elevation(el));
        settle(&mut node);
        assert!(
            near(node.aim.current(), (-45.0, 30.0)),
            "raw ({az}, {el}): aimed at {:?}",
            node.aim.current()
        );
        aim(&node.param_set(), Azimuth::new(-45.0), Elevation::new(30.0));
        settle(&mut node);
        assert!(
            near(node.aim.current(), (-45.0, 30.0)),
            "did not recover from raw ({az}, {el}): {:?}",
            node.aim.current()
        );
    }
    // A reset (and a fork's) seats on the held position, not on a NaN.
    aim(
        &node.param_set(),
        Azimuth::new(f32::NAN),
        Elevation::new(f32::NAN),
    );
    Node::reset(&mut node);
    assert!(
        near(node.aim.current(), (-45.0, 30.0)),
        "{:?}",
        node.aim.current()
    );
}

#[test]
fn rejects_garbage_bytes() {
    let err = HrtfBinauralNode::new(&[0u8; 16], SampleRate::new(44_100.0));
    assert!(err.is_err(), "non-HRIR bytes must fail to construct");
}

#[test]
fn produces_output_after_one_frame_of_latency() {
    let mut node = make_node();
    aim(&node.param_set(), Azimuth::new(90.0), Elevation::new(0.0));

    // Drive a steady tone through more than one step and confirm the
    // renderer eventually emits non-silence (output lags by one step).
    let mut produced_nonzero = false;
    for n in 0..(tutti_dsp::HRTF_STEP * 2) {
        let s = ((n as f32) * 0.05).sin();
        let out = tick(&mut node, [s, s]);
        if out[0].abs() > 1e-6 || out[1].abs() > 1e-6 {
            produced_nonzero = true;
        }
    }
    assert!(
        produced_nonzero,
        "HRTF node should emit audio after warm-up"
    );
}

/// Run `node` over `frames` frames with a unit impulse on both inputs at
/// frame `at`, in 64-frame blocks (the hot path, not one frame at a
/// time). Returns the (left, right) outputs.
fn impulse_response(node: &mut HrtfBinauralNode, at: usize, frames: usize) -> (Vec<f32>, Vec<f32>) {
    let (mut l, mut r) = (Vec::new(), Vec::new());
    let mut done = 0;
    while done < frames {
        let n = BLOCK.min(frames - done);
        let input: Vec<f32> = (0..n)
            .map(|i| if done + i == at { 1.0 } else { 0.0 })
            .collect();
        let out = drive(node, RATE, &[&input, &input], &[]);
        l.extend_from_slice(&out[0]);
        r.extend_from_slice(&out[1]);
        done += n;
    }
    (l, r)
}

fn onsets(ch: &[f32]) -> Vec<usize> {
    ch.iter()
        .enumerate()
        .filter(|(_, s)| s.abs() > 1e-4)
        .map(|(i, _)| i)
        .collect()
}

/// The declared latency must be the delay the output really has, on both
/// outputs, or PDC would not compensate a binaural track.
///
/// The figure is measured: one step (128). The synthetic sphere's
/// HRIRs are deltas at t = 0, so every frame of delay seen here is the
/// step's.
///
/// Impulses land at several offsets within a step (first, second, last,
/// and across a step boundary), because a renderer whose delay depended on
/// the in-step phase would have no single latency to report.
#[test]
fn reported_latency_is_the_measured_impulse_delay() {
    use tutti_dsp::{HRTF_LATENCY as LATENCY, HRTF_STEP as STEP};
    let node = make_node();
    assert_eq!(node.shape().latency, Latency::new(Samples(STEP)));

    for at in [0, 1, STEP - 1, STEP, 2 * STEP + 37] {
        let mut node = make_node();
        aim(&node.param_set(), Azimuth(90.0), Elevation::LEVEL);
        Node::reset(&mut node);
        let (l, r) = impulse_response(&mut node, at, at + 3 * STEP);
        assert_eq!(onsets(&l), vec![at + LATENCY], "left, impulse at {at}");
        assert_eq!(onsets(&r), vec![at + LATENCY], "right, impulse at {at}");
    }
}

/// Through PDC: a binaural track on outputs 0/1 beside a dry path on 2,
/// in a graph. The compiler must delay the dry path by the
/// declared latency, so an impulse leaves all three outputs on one frame.
#[test]
fn pdc_compensates_the_other_path_by_the_binaural_latency() {
    use tutti_dsp::HRTF_LATENCY as LATENCY;
    use tutti_graph::GraphBuilder;
    use tutti_nodes::testing::Through;

    let mut node = make_node();
    aim(&node.param_set(), Azimuth(90.0), Elevation::LEVEL);
    Node::reset(&mut node);
    let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::from_count(3));
    let (hrtf, _) = g.add_with_controls(node);
    let dry = g.add(Through::mono());
    g.connect_input(0, hrtf, 0);
    g.connect_input(0, hrtf, 1);
    g.connect_input(0, dry, 0);
    g.connect_output(hrtf, 0, 0);
    g.connect_output(hrtf, 1, 1);
    g.connect_output(dry, 0, 2);
    let mut r = g
        .renderer(Prepare::new(RATE, MaxBlock::of(Samples(BLOCK))))
        .expect("builds");
    let mut impulse = vec![0.0f32; 2 * LATENCY];
    impulse[0] = 1.0;
    let out = r.render_input(&[&impulse]);
    for (o, ch) in out.iter().enumerate() {
        assert_eq!(onsets(ch), vec![LATENCY], "output {o}");
    }
}

/// The dry half of the blend leaves with the wet half, so the reported
/// latency is true of the whole output at any blend. Blending the undelayed
/// mono against a wet signal a step late would break that.
#[test]
fn dry_and_wet_leave_together_at_every_blend() {
    use tutti_dsp::{HRTF_LATENCY as LATENCY, HRTF_STEP as STEP};
    for blend in [0.0_f32, 0.5, 1.0] {
        let mut node = make_node();
        aim(&node.param_set(), Azimuth(90.0), Elevation::LEVEL);
        node.blend.store(Mix(blend));
        Node::reset(&mut node);
        let at = 100;
        let (l, r) = impulse_response(&mut node, at, at + 2 * STEP);
        assert_eq!(onsets(&l), vec![at + LATENCY], "left, blend {blend}");
        assert_eq!(onsets(&r), vec![at + LATENCY], "right, blend {blend}");
        if blend == 0.0 {
            // Fully dry is the mono fold, delayed and otherwise untouched.
            assert!((l[at + LATENCY] - 1.0).abs() < 1e-6);
            assert!((r[at + LATENCY] - 1.0).abs() < 1e-6);
        }
    }
}

/// A blend that is NaN (`Mix::new_clamped` passes a NaN through) renders as
/// the blend it replaced, never as NaN output.
#[test]
fn a_nan_blend_holds_the_last_one() {
    let mut node = make_node();
    aim(&node.param_set(), Azimuth(90.0), Elevation::LEVEL);
    node.blend.store(Mix(0.5));
    let (l0, _) = impulse_response(&mut node, 0, 4 * tutti_dsp::HRTF_STEP);
    node.blend.store(Mix(f32::NAN));
    let (l, r) = impulse_response(&mut node, 0, 4 * tutti_dsp::HRTF_STEP);
    assert!(
        l.iter().chain(&r).all(|s| s.is_finite()),
        "NaN blend reached the output"
    );
    assert_eq!(l, l0, "the blend should have held at 0.5");
}

/// Same contract as the VBAP panner's: `reset` clears the streaming
/// buffers and the de-zipper ramp, never the caller's placement. A fork is
/// reset before it renders, and a clone shares these atomics, so a reset
/// that re-aimed would move the live source too.
#[test]
fn reset_keeps_the_authored_placement() {
    let mut node = make_node();
    aim(&node.param_set(), Azimuth(45.0), Elevation(10.0));
    node.blend.store(Mix(0.4));

    Node::reset(&mut node);

    assert_eq!(node.azimuth.load(), Azimuth(45.0));
    assert_eq!(node.elevation.load(), Elevation(10.0));
    assert!(
        (node.blend.load().get() - 0.4).abs() < 0.001,
        "reset changed the blend to {}",
        node.blend.load().get()
    );
}

/// The other half of the contract: the streaming state *is* dropped, so a
/// reset between takes does not bleed the previous take's convolution tail
/// into the next one — which is the whole reason a host calls `reset`.
#[test]
fn reset_drops_the_convolution_tail() {
    let mut node = make_node();
    aim(&node.param_set(), Azimuth(90.0), Elevation::LEVEL);

    // Fill the step and the input history with a loud take.
    for n in 0..(tutti_dsp::HRTF_STEP * 3) {
        let s = ((n as f32) * 0.05).sin();
        tick(&mut node, [s, s]);
    }

    Node::reset(&mut node);

    // Silence in. With the tail dropped the frames that follow are silent
    // too; a retained tail would ring out through them.
    let mut peak = 0.0f32;
    for _ in 0..(tutti_dsp::HRTF_STEP * 2) {
        let out = tick(&mut node, [0.0, 0.0]);
        peak = peak.max(out[0].abs()).max(out[1].abs());
    }
    assert!(
        peak < 1e-6,
        "reset left {peak} of the previous take's tail in the buffers"
    );
}

/// `Clone` shares the position atomics rather than snapshotting them —
/// parity with [`VbapPannerNode`](crate::spatial::VbapPannerNode), and the
/// reason `reset` must never write them (see `reset_keeps_the_authored_placement`).
/// A clone is the fork source's template, taken at insert, so a
/// snapshotting clone would silently freeze every later export at the
/// bearing set at insert. (The fork itself detaches; see
/// `tests/spatial_param_fork.rs`.)
#[test]
fn clone_shares_the_position_atomics() {
    let node = make_node();
    let c = node.clone();

    // Set through the original, observe through the clone.
    aim(&node.param_set(), Azimuth::new(45.0), Elevation::new(10.0));
    assert_eq!(c.azimuth.load(), Azimuth(45.0));
    assert_eq!(c.elevation.load(), Elevation(10.0));

    // And back the other way.
    aim(&c.param_set(), Azimuth::new(-60.0), Elevation::new(-20.0));
    assert_eq!(node.azimuth.load(), Azimuth(-60.0));
    assert_eq!(node.elevation.load(), Elevation(-20.0));
}

/// Two graph rates above [`MAX_PREPARED_RATE`] share one prepared set, but
/// the de-zipper still glides in the graph's own time: built at 1.536 MHz
/// and prepared at 1.024 MHz, 50 ms is 51 200 samples, exactly 400 steps,
/// and leaves `e^-1` of a move. Left at 1.536 MHz's coefficient (600 steps
/// per 50 ms) it would leave `e^-(2/3)` = 0.51.
#[test]
fn a_prepare_above_the_ceiling_retunes_the_glide() {
    assert_eq!(HRTF_STEP, 128, "the rates below assume 128-sample steps");
    let mut node =
        HrtfBinauralNode::new(&synthetic_hrir_sphere(48_000, 4), SampleRate(1_536_000.0))
            .expect("parses");
    node.prepare(&Prepare::new(
        SampleRate(1_024_000.0),
        MaxBlock::of(Samples(64)),
    ));

    let e_inv = (-1.0f32).exp();
    let left = remaining_after(&mut node.aim, 400);
    assert!(
        (left - e_inv).abs() < 1e-4,
        "after 50 ms at 1.024 MHz {left} of the move is left, expected e^-1 = {e_inv}"
    );
}

/// Remaining fraction of a step from 0 to 90 degrees of height after
/// `steps` rendered steps.
fn remaining_after(aim: &mut AngleSmoother, steps: usize) -> f32 {
    aim.reset_to_target(Direction::new(Azimuth::FRONT, Elevation(0.0)));
    for _ in 0..steps {
        aim.step(Direction::new(Azimuth::FRONT, Elevation(90.0)));
    }
    (90.0 - aim.current().elevation.get()) / 90.0
}

/// The de-zipper's time constant is 50 ms of audio: after 50 ms of
/// steps a move has `e^-1` of its distance left. At 51.2 kHz, 50 ms is
/// 2560 samples, exactly twenty 128-sample steps; at 102.4 kHz (after a
/// retune) it is forty.
#[test]
fn the_glide_has_a_50_ms_time_constant_in_audio_time() {
    assert_eq!(HRTF_STEP, 128, "the rates below assume 128-sample steps");
    let e_inv = (-1.0f32).exp();

    let mut aim = AngleSmoother::new(SampleRate(51_200.0), Samples(HRTF_STEP));
    let left = remaining_after(&mut aim, 20);
    assert!(
        (left - e_inv).abs() < 1e-4,
        "after 50 ms at 51.2 kHz {left} of the move is left, expected e^-1 = {e_inv}"
    );

    aim.set_sample_rate(SampleRate(102_400.0));
    let left = remaining_after(&mut aim, 40);
    assert!(
        (left - e_inv).abs() < 1e-4,
        "after 50 ms at 102.4 kHz {left} of the move is left, expected e^-1 = {e_inv}"
    );
}

/// The ports a lane can drive are the params the node declares, each with
/// its cell as the base the graph's modulation rides on.
#[test]
fn the_renderer_declares_its_params() {
    let node = make_node();
    node.azimuth.store(Azimuth(10.0));
    node.elevation.store(Elevation(20.0));
    node.blend.store(Mix(0.3));
    assert_eq!(node.shape().params.as_slice(), &HRTF_BINAURAL_PARAMS[..]);
    let bases: Vec<_> = (0..4).map(|k| node.param_base(k)).collect();
    assert_eq!(
        bases,
        [Some(10.0), Some(20.0), Some(0.3), None],
        "each port's base is its cell"
    );
}

/// A lane that drives a param is heard as the cell would be: a node whose
/// lane holds the value, over a cell at another, settles on the render of a
/// node whose cell holds it, for each of the three ports.
///
/// The value must change the render from the default's, else the row would
/// pass on a node that ignored both the lane and the cell.
#[test]
fn a_lane_drives_each_param_as_its_cell_would() {
    let input = [0.5f32; BLOCK];
    let settle = |node: &mut HrtfBinauralNode, lane: Option<(usize, f32)>| {
        let held = lane.map(|(_, v)| [v; BLOCK]);
        let mut params: [Option<&[f32]>; 3] = [None; 3];
        if let Some((k, _)) = lane {
            params[k] = held.as_ref().map(|v| &v[..]);
        }
        // Ten 50 ms time constants of blocks.
        let mut last = Vec::new();
        for _ in 0..(RATE.get() as usize / 2).div_ceil(BLOCK) {
            last = drive(node, RATE, &[&input, &input], &params);
        }
        last.iter().map(|c| c[BLOCK - 1]).collect::<Vec<f32>>()
    };
    for (k, param, value) in [
        (0, UnitParam::Azimuth, 90.0f32),
        (1, UnitParam::Elevation, 45.0),
        (2, UnitParam::Wet, 0.0),
    ] {
        let mut default = make_node();
        let at_default = settle(&mut default, None);

        let mut cell = make_node();
        assert!(cell.param_set().set(param, value));
        Node::reset(&mut cell);
        let at_cell = settle(&mut cell, None);
        assert!(
            at_cell
                .iter()
                .zip(&at_default)
                .any(|(a, b)| (a - b).abs() > 1e-3),
            "{param:?}: the value does not change the render, so the row proves nothing"
        );

        let mut lane = make_node();
        let at_lane = settle(&mut lane, Some((k, value)));
        for (ch, (a, b)) in at_lane.iter().zip(&at_cell).enumerate() {
            assert!(
                (a - b).abs() < 1e-3,
                "{param:?}, channel {ch}: the lane settled at {a}, the cell at {b}"
            );
        }
    }
}
