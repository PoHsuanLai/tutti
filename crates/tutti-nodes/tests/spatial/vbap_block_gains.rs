//! Block-boundary behaviour of `VbapPannerNode`'s ramped render.
//!
//! The panner solves once per block and linearly ramps the gain vector from
//! the previous block's end to this block's end. The one-pole response of the
//! position is pinned analytically in `panner/tests.rs`
//! (`solve_block_follows_the_exact_one_pole_response`); what is pinned here is
//! what only the node's render can show: with DC in, the output is the gain
//! vector itself, so the ramp must be continuous across blocks, a width
//! crossing the mono-fold threshold must crossfade, and a position change must
//! land in the next block as a ramp that ends on the per-frame solve.

use tutti_graph::Node;
use tutti_graph::ParamNode;
use tutti_graph::contract::{drive, prepared};
use tutti_nodes::VbapPannerNode;
use tutti_nodes::testing::aim;
use tutti_types::{Azimuth, ChannelLayout, ChannelTopology, Elevation, SampleRate, UnitParam};

const BLOCK: usize = 64;
const EDGE_TOL: f32 = 1e-5;
const RATE: SampleRate = SampleRate(48_000.0);

/// A two-channel block, one `Vec` per input.
type Block = [Vec<f32>; 2];

/// `node`, prepared at 48 kHz for blocks of up to [`BLOCK`] frames (what
/// `set_sample_rate` was).
fn prep(node: VbapPannerNode) -> VbapPannerNode {
    prepared(node, RATE, BLOCK)
}

/// One block through the node, one `Vec` per output channel.
fn process(node: &mut VbapPannerNode, input: &Block) -> Vec<Vec<f32>> {
    drive(node, RATE, &[&input[0], &input[1]], &[])
}

/// A block of one frame, written to `out`.
fn tick(node: &mut VbapPannerNode, input: [f32; 2], out: &mut [f32]) {
    let rendered = drive(node, RATE, &[&input[..1], &input[1..]], &[]);
    for (o, c) in out.iter_mut().zip(rendered) {
        *o = c[0];
    }
}

/// Deterministic, decorrelated L/R input, so the width path's two virtual
/// A constant `(l, r)` block, so every output frame *is* a gain readout.
fn dc_block(l: f32, r: f32) -> Block {
    [vec![l; BLOCK], vec![r; BLOCK]]
}

/// Render `blocks` blocks of DC through `node`, calling `per_block` before
/// each, and return every channel's frames concatenated across blocks.
fn dc_stream(
    node: &mut VbapPannerNode,
    input: &Block,
    blocks: usize,
    per_block: impl Fn(&VbapPannerNode, usize),
) -> Vec<Vec<f32>> {
    let mut stream = vec![Vec::new(); node.channels().count() as usize];
    for b in 0..blocks {
        per_block(node, b);
        for (s, out) in stream.iter_mut().zip(process(node, input)) {
            s.extend(out);
        }
    }
    stream
}

/// Assert no block boundary steps further than the ramp inside the block it
/// opens. A linear ramp's frame-to-frame step is constant within a block, and
/// the boundary is one more step of the *next* block's ramp — so a boundary
/// step larger than that block's in-block step is a discontinuity.
fn assert_boundaries_are_on_the_ramp(stream: &[Vec<f32>], what: &str) {
    for (c, s) in stream.iter().enumerate() {
        for b in 1..s.len() / BLOCK {
            let start = b * BLOCK;
            let boundary = (s[start] - s[start - 1]).abs();
            let in_block = (s[start + 1] - s[start]).abs();
            assert!(
                boundary <= in_block + 1e-6,
                "{what}: ch {c} steps {boundary} at the block {b} boundary, \
                 but only {in_block} per frame inside the block"
            );
        }
    }
}

/// With DC in, the output is the gain vector itself; across a jump and a
/// per-block sweep it must be continuous over block boundaries.
#[test]
fn the_gain_ramp_is_continuous_across_block_boundaries() {
    for speakers in [2u16, 6] {
        for width in [0.0f32, 1.0] {
            let mut node = prep(
                VbapPannerNode::for_topology(&ChannelTopology::for_width(ChannelLayout::from(
                    speakers,
                )))
                .unwrap(),
            );
            node.param_set().set(UnitParam::StereoWidth, width);
            aim(&node.param_set(), Azimuth(-20.0), Elevation::LEVEL);
            Node::reset(&mut node);
            let stream = dc_stream(&mut node, &dc_block(1.0, 0.5), 24, |n, b| {
                // A 140° jump, then a sweep that moves the target every block.
                let az = if b < 12 {
                    120.0
                } else {
                    120.0 - 15.0 * (b - 11) as f32
                };
                aim(&n.param_set(), Azimuth(az), Elevation::LEVEL);
            });
            assert_boundaries_are_on_the_ramp(
                &stream,
                &format!("{speakers} speakers, width {width}"),
            );
        }
    }
}

/// A width crossing the mono-fold threshold between blocks crossfades the two
/// branches across the next block instead of stepping at the boundary.
///
/// A per-frame solve steps by 0.64 of full scale at every crossing of
/// `stereo_width_switch`.
#[test]
fn a_width_crossing_the_fold_threshold_crossfades() {
    let mut node = prep(VbapPannerNode::stereo());
    aim(&node.param_set(), Azimuth(-15.0), Elevation::LEVEL);
    Node::reset(&mut node);
    // L only: the two-source branch and the fold weigh it differently.
    let stream = dc_stream(&mut node, &dc_block(1.0, 0.0), 8, |n, b| {
        n.param_set()
            .set(UnitParam::StereoWidth, if b % 2 == 0 { 1.0 } else { 0.0 });
    });
    // The crossings must actually move the output, or the continuity check
    // below would pass on a node that ignores width.
    let moved = (stream[0][2 * BLOCK - 1] - stream[0][BLOCK - 1]).abs();
    assert!(
        moved > 0.1,
        "the width switch should change the gains, moved {moved}"
    );
    assert_boundaries_are_on_the_ramp(&stream, "width switch");
}

/// A position written between blocks takes effect in the next block, ramped:
/// the block starts one ramp step from where the previous block ended, and
/// its last frame is the per-frame solve at that frame, within `EDGE_TOL`.
///
/// The reference is the same scenario driven one frame at a time (one-frame
/// blocks, so one 1-step advance per frame), compared within `EDGE_TOL`.
///
/// What this pins: a one-block advance of the smoother equals the same 64
/// steps taken one at a time, to within `EDGE_TOL`; the block starts one ramp
/// step from the previous end; a moved target takes effect in the next block.
/// What it does not pin: that the last frame is the *exact* per-frame solve,
/// and the smoother's timing and coefficient. The analytic panner test
/// `solve_block_follows_the_exact_one_pole_response` pins those (position
/// closed form, and the gains solved at the advanced position).
#[test]
fn a_position_change_lands_in_the_next_block_as_a_ramp() {
    let setup = || {
        let mut n = prep(VbapPannerNode::surround_5_1());
        aim(&n.param_set(), Azimuth(0.0), Elevation::LEVEL);
        Node::reset(&mut n);
        n
    };
    let input = dc_block(0.8, 0.8);

    let mut node = setup();
    let out = process(&mut node, &input);
    let settled: Vec<f32> = (0..6).map(|c| out[c][BLOCK - 1]).collect();
    // Seated and unchanged: the first block is flat.
    for (c, &s) in settled.iter().enumerate() {
        assert_eq!(out[c][0], s, "ch {c} moved without a change");
    }

    aim(&node.param_set(), Azimuth(90.0), Elevation::LEVEL);
    let out = process(&mut node, &input);

    let mut reference = setup();
    let mut frame = [0.0f32; 6];
    for _ in 0..BLOCK {
        tick(&mut reference, [0.8, 0.8], &mut frame);
    }
    aim(&reference.param_set(), Azimuth(90.0), Elevation::LEVEL);
    for _ in 0..BLOCK {
        tick(&mut reference, [0.8, 0.8], &mut frame);
    }

    let mut any_moved = false;
    for (c, &s) in settled.iter().enumerate() {
        let last = out[c][BLOCK - 1];
        assert!(
            (last - frame[c]).abs() <= EDGE_TOL,
            "ch {c}: the block must end on the per-frame solve: {last} vs {}",
            frame[c]
        );
        let step = (last - s) / BLOCK as f32;
        let first = out[c][0];
        assert!(
            (first - (s + step)).abs() <= 1e-6,
            "ch {c}: the block must start one ramp step from {s}, got {first}"
        );
        any_moved |= (last - s).abs() > 1e-3;
    }
    assert!(any_moved, "the new position must take effect in this block");
}
