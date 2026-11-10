use super::*;
use tutti_graph::contract::{drive, prepared};

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
/// at the wrong place. Without `hold_finite` the smoother's state turns
/// NaN on the first step and stays NaN through every later target.
///
/// Mutation (run): `placement` returning the raw cells
/// (`self.controls.target.load()`) → the aim is NaN while the position
/// is NaN and never returns to 90° → fails.
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
    let near = |(az, el): (Azimuth, Elevation), want: (f32, f32)| {
        (az.get() - want.0).abs() < 0.01 && (el.get() - want.1).abs() < 0.01
    };
    node.controls().set_position(-45.0, 30.0);
    settle(&mut node);
    assert!(
        near(node.panner.aim(), (-45.0, 30.0)),
        "{:?}",
        node.panner.aim()
    );
    // An infinite bearing wraps to NaN on the way into the cell; an
    // infinite height is clamped to the pole there, so it is a position.
    for (az, el) in [(f32::NAN, 0.0), (0.0, f32::NAN), (f32::INFINITY, 10.0)] {
        // Only the bad coordinate is ignored; the other moves.
        let want = (
            if az.is_finite() { az } else { -45.0 },
            if el.is_finite() { el } else { 30.0 },
        );
        node.controls().set_position(az, el);
        settle(&mut node);
        assert!(
            near(node.panner.aim(), want),
            "({az}, {el}): aimed at {:?}, want {want:?}",
            node.panner.aim()
        );
        node.controls().set_position(-45.0, 30.0);
        settle(&mut node);
        assert!(
            near(node.panner.aim(), (-45.0, 30.0)),
            "did not recover from ({az}, {el}): {:?}",
            node.panner.aim()
        );
    }
    // `set_position` normalizes, so ±∞ reaches the cell only by a raw
    // store; `hold_finite` must still refuse it there.
    //
    // Mutation (run): `hold_finite`'s `is_finite` → `!is_nan` → the aim
    // goes to ∞ and does not recover → fails.
    for (az, el) in [
        (f32::INFINITY, 30.0),
        (f32::NEG_INFINITY, 30.0),
        (-45.0, f32::INFINITY),
        (-45.0, f32::NEG_INFINITY),
    ] {
        let target = &node.controls.target;
        target.azimuth.store(Azimuth(az));
        target.elevation.store(Elevation(el));
        settle(&mut node);
        assert!(
            near(node.panner.aim(), (-45.0, 30.0)),
            "raw ({az}, {el}): aimed at {:?}",
            node.panner.aim()
        );
        node.controls().set_position(-45.0, 30.0);
        settle(&mut node);
        assert!(
            near(node.panner.aim(), (-45.0, 30.0)),
            "did not recover from raw ({az}, {el}): {:?}",
            node.panner.aim()
        );
    }
    // A reset (and a fork's) seats on the held position, not on a NaN.
    node.controls().set_position(f32::NAN, f32::NAN);
    Node::reset(&mut node);
    assert!(
        near(node.panner.aim(), (-45.0, 30.0)),
        "{:?}",
        node.panner.aim()
    );
}

#[test]
fn rejects_garbage_bytes() {
    let err = HrtfBinauralNode::new(&[0u8; 16], 44_100.0);
    assert!(err.is_err(), "non-HRIR bytes must fail to construct");
}

#[test]
fn produces_output_after_one_frame_of_latency() {
    let mut node = make_node();
    node.controls().set_position(90.0, 0.0);

    // Drive a steady tone through more than one step and confirm the
    // renderer eventually emits non-silence (output lags by one step).
    let mut produced_nonzero = false;
    for n in 0..(crate::hrtf::panner::STEP * 2) {
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
///
/// Mutation (run): declaring `Latency::ZERO` in `shape` fails the
/// declaration assertion (reports 0). Mutation: `LATENCY = STEP - 1`
/// fails every impulse (declared one frame early).
#[test]
fn reported_latency_is_the_measured_impulse_delay() {
    use crate::hrtf::panner::{LATENCY, STEP};
    let node = make_node();
    assert_eq!(node.shape().latency, Latency::new(Samples(STEP)));

    for at in [0, 1, STEP - 1, STEP, 2 * STEP + 37] {
        let mut node = make_node();
        node.controls()
            .set_position(Azimuth(90.0), Elevation::LEVEL);
        Node::reset(&mut node);
        let (l, r) = impulse_response(&mut node, at, at + 3 * STEP);
        assert_eq!(onsets(&l), vec![at + LATENCY], "left, impulse at {at}");
        assert_eq!(onsets(&r), vec![at + LATENCY], "right, impulse at {at}");
    }
}

/// Through PDC: a binaural track on outputs 0/1 beside a dry path on 2,
/// in a graph. The compiler must delay the dry path by the
/// declared latency, so an impulse leaves all three outputs on one frame.
///
/// Mutation (run): declaring `Latency::ZERO` in `shape` → the dry onset
/// leaves at frame 0, `LATENCY` early → fails.
#[test]
fn pdc_compensates_the_other_path_by_the_binaural_latency() {
    use crate::hrtf::panner::LATENCY;
    use tutti_graph::GraphBuilder;
    use tutti_nodes::testing::Through;

    let mut node = make_node();
    node.controls()
        .set_position(Azimuth(90.0), Elevation::LEVEL);
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
        .renderer(Prepare::new(RATE, Samples(BLOCK)))
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
///
/// Mutation: blending against the incoming mono instead of the delayed
/// `dry` fails blend 0.0 and 0.5 (an onset at the impulse itself).
/// Mutation: dropping the `dry.copy_from_slice` in `render_step` fails
/// them too (the dry step is never refreshed, so the dry half never
/// arrives).
#[test]
fn dry_and_wet_leave_together_at_every_blend() {
    use crate::hrtf::panner::{LATENCY, STEP};
    for blend in [0.0_f32, 0.5, 1.0] {
        let mut node = make_node();
        node.controls()
            .set_position(Azimuth(90.0), Elevation::LEVEL);
        node.controls().set_blend(Mix(blend));
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
///
/// Mutation (run): reading `self.controls.blend.load()` in `process` without
/// the `hold_finite` makes the NaN case render NaN (it failed that way before the fix).
#[test]
fn a_nan_blend_holds_the_last_one() {
    let mut node = make_node();
    node.controls()
        .set_position(Azimuth(90.0), Elevation::LEVEL);
    node.controls().set_blend(Mix(0.5));
    let (l0, _) = impulse_response(&mut node, 0, 4 * crate::hrtf::panner::STEP);
    node.controls().set_blend(Mix(f32::NAN));
    let (l, r) = impulse_response(&mut node, 0, 4 * crate::hrtf::panner::STEP);
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
    node.controls().set_position(Azimuth(45.0), Elevation(10.0));
    node.controls().set_blend(Mix(0.4));

    Node::reset(&mut node);

    assert_eq!(node.controls().azimuth(), Azimuth(45.0));
    assert_eq!(node.controls().elevation(), Elevation(10.0));
    assert!(
        (node.controls().blend().get() - 0.4).abs() < 0.001,
        "reset changed the blend to {}",
        node.controls().blend().get()
    );
}

/// The other half of the contract: the streaming state *is* dropped, so a
/// reset between takes does not bleed the previous take's convolution tail
/// into the next one — which is the whole reason a host calls `reset`.
#[test]
fn reset_drops_the_convolution_tail() {
    let mut node = make_node();
    node.controls()
        .set_position(Azimuth(90.0), Elevation::LEVEL);

    // Fill the step and the input history with a loud take.
    for n in 0..(crate::hrtf::panner::STEP * 3) {
        let s = ((n as f32) * 0.05).sin();
        tick(&mut node, [s, s]);
    }

    Node::reset(&mut node);

    // Silence in. With the tail dropped the frames that follow are silent
    // too; a retained tail would ring out through them.
    let mut peak = 0.0f32;
    for _ in 0..(crate::hrtf::panner::STEP * 2) {
        let out = tick(&mut node, [0.0, 0.0]);
        peak = peak.max(out[0].abs()).max(out[1].abs());
    }
    assert!(
        peak < 1e-6,
        "reset left {peak} of the previous take's tail in the buffers"
    );
}

/// `Clone` shares the position atomics rather than snapshotting them —
/// parity with [`VbapPannerNode`](crate::vbap::VbapPannerNode), and the
/// reason `reset` must never write them (see `reset_keeps_the_authored_placement`).
/// A clone is the fork source's template, taken at insert, so a
/// snapshotting clone would silently freeze every later export at the
/// bearing set at insert. (The fork itself detaches; see
/// `tests/isolate_snapshots.rs`.)
#[test]
fn clone_shares_the_position_atomics() {
    let node = make_node();
    let c = node.clone();

    // Set through the original, observe through the clone.
    node.controls().set_position(45.0, 10.0);
    assert_eq!(c.controls().azimuth(), Azimuth(45.0));
    assert_eq!(c.controls().elevation(), Elevation(10.0));

    // And back the other way.
    c.controls().set_position(-60.0, -20.0);
    assert_eq!(node.controls().azimuth(), Azimuth(-60.0));
    assert_eq!(node.controls().elevation(), Elevation(-20.0));
}
