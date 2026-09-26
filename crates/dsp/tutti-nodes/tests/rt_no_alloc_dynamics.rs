//! Regression gate: dynamics processors must not allocate per-buffer.
//!
//! Covers the four `tutti-nodes` dynamics nodes: `CompressorNode` (mono +
//! stereo), `GateNode` (mono + stereo), `LimiterNode` (lookahead), and
//! `BrickwallLimiterNode` (zero-latency clipper). Each runs natively, alone
//! in a graph through `tutti_graph::contract::BlockRig`, so the gate walks
//! the executor's block path as well as the node's.
//!
//! Lookahead limiter is the most failure-prone of the group — it carries
//! a monotonic deque + a ring buffer, both of which would historically
//! be tempting to resize on `set_lookahead`. The gate here covers steady
//! blocks only; lookahead changes are off-RT (`prepare`).
//!
//! Mutation (run): size the compressor's gain lane in `process` rather
//! than `prepare` (`self.gains = vec![0.0; size]` at the top of `process`)
//! → both compressor gates fail on the first measured block.

use assert_no_alloc::AllocDisabler;
use tutti_core::{ChannelLayout, SampleRate};
use tutti_graph::contract::BlockRig;
use tutti_graph::IntoNode;
use tutti_nodes::{BrickwallLimiterNode, CompressorNode, GateNode, LimiterNode};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// `node` alone in a graph through `BlockRig`: every input an alternating
/// ±`amplitude` (to keep the envelope follower active), `warm` warm-up
/// blocks, then `blocks` 64-frame blocks under `assert_no_alloc`.
fn gate<N: IntoNode>(node: N, amplitude: f32, warm: usize, blocks: usize) {
    let (mut rig, _controls) = BlockRig::new(node, SampleRate(48_000.0), 64);
    for c in rig.inputs_mut() {
        for (i, x) in c.iter_mut().enumerate() {
            *x = if i % 2 == 0 { amplitude } else { -amplitude };
        }
    }
    for _ in 0..warm {
        rig.block();
    }
    assert!(
        rig.output(0).iter().any(|s| s.abs() > 1e-6),
        "the node rendered silence; the gate would walk no DSP"
    );
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..blocks {
            rig.block();
        }
    });
}

#[test]
fn compressor_mono_process_is_allocation_free() {
    // 2 inputs (audio + sidechain), 1 output.
    gate(
        CompressorNode::mono(-20.0, 4.0, 0.005, 0.050),
        0.7,
        16,
        2_000,
    );
}

#[test]
fn compressor_stereo_process_is_allocation_free() {
    // 4 inputs (L, R, SC-L, SC-R), 2 outputs, linked gain.
    gate(
        CompressorNode::stereo(-18.0, 3.0, 0.003, 0.080).with_soft_knee(6.0),
        0.8,
        16,
        2_000,
    );
}

#[test]
fn limiter_node_process_is_allocation_free() {
    // Warm-up fills the lookahead ring and primes the deque; over ceiling.
    gate(
        LimiterNode::new(-3.0, -0.3).with_lookahead(0.005),
        1.5,
        32,
        2_000,
    );
}

#[test]
fn limiter_node_wide_6ch_process_is_allocation_free() {
    // The per-channel lookahead rings + frame scratch must be built before
    // the first block; the linked-gain wide path must not allocate per
    // buffer.
    gate(
        LimiterNode::with_channels(ChannelLayout::from(6u16), -3.0, -0.3).with_lookahead(0.005),
        1.5,
        32,
        2_000,
    );
}

#[test]
fn brickwall_limiter_wide_6ch_process_is_allocation_free() {
    gate(
        BrickwallLimiterNode::with_channels(ChannelLayout::from(6u16), -0.3),
        1.5,
        1,
        5_000,
    );
}

#[test]
fn brickwall_limiter_process_is_allocation_free() {
    gate(BrickwallLimiterNode::new(-0.3), 1.5, 1, 5_000);
}

#[test]
fn gate_mono_process_is_allocation_free() {
    // 2 inputs (audio + sidechain), 1 output.
    gate(
        GateNode::mono(-40.0, 0.001, 0.010, 0.100).with_range(-60.0),
        0.5,
        16,
        2_000,
    );
}

#[test]
fn gate_stereo_process_is_allocation_free() {
    gate(GateNode::stereo(-40.0, 0.001, 0.010, 0.100), 0.6, 16, 2_000);
}
