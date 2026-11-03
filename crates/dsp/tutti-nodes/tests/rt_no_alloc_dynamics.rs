//! Regression gate: dynamics processors must not allocate per-buffer.
//!
//! Covers the four `tutti-nodes` dynamics nodes: `CompressorNode` (mono +
//! stereo), `GateNode` (mono + stereo), `LimiterNode` (lookahead), and
//! `BrickwallLimiterNode` (zero-latency clipper). Each runs alone
//! in a graph through `tutti_graph::contract::BlockRig`, so the gate walks
//! the executor's block path as well as the node's.
//!
//! Lookahead limiter is the most failure-prone of the group — it carries
//! a monotonic deque + a ring buffer, both of which would historically
//! be tempting to resize on `set_lookahead`. Its steady gates warm up
//! outside the measurement; a burst and its release are measured from the
//! first block, which is where the deque fills to its whole window.
//! Lookahead changes are off-RT (`prepare`).
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

/// A lookahead limiter through a burst and its release, every block under
/// `assert_no_alloc` from the first. Whenever the envelope rises (from the
/// node's initial zero onto the burst, and through the release after it)
/// every gain is larger than the last, so the window-minimum deque keeps
/// every push and fills to the whole lookahead window. The constant-drive
/// gates above warm up outside the gate, so the one fill that would grow
/// the deque happens before they measure.
///
/// Mutation (run): push into the deque before evicting the entry leaving
/// the window (`MonotonicMinDeque::push`'s old order) → the deque reaches
/// window + 1 entries and grows inside the gate.
#[test]
fn limiter_node_burst_and_release_are_allocation_free() {
    let node = LimiterNode::new(-3.0, -0.3)
        .with_lookahead(0.005)
        .with_release(0.1);
    let (mut rig, _controls) = BlockRig::new(node, SampleRate(48_000.0), 64);
    let fill = |rig: &mut BlockRig, amplitude: f32| {
        for c in rig.inputs_mut() {
            for (i, x) in c.iter_mut().enumerate() {
                *x = if i % 2 == 0 { amplitude } else { -amplitude };
            }
        }
    };
    // 0.1 s of release at 48 kHz is 75 blocks of 64: each phase runs well
    // past the 240-frame window, so the deque fills and turns over.
    assert_no_alloc::assert_no_alloc(|| {
        fill(&mut rig, 4.0);
        for _ in 0..200 {
            rig.block();
        }
        fill(&mut rig, 0.05);
        for _ in 0..200 {
            rig.block();
        }
    });
    assert!(
        rig.output(0).iter().any(|s| s.abs() > 1e-6),
        "the limiter rendered silence after the release"
    );
}

/// The brickwall with its ceiling port fed a per-frame ramp (the path that
/// writes each frame's linear ceiling into channel 0 and clips it last),
/// 6 channels, 1 000 blocks under `assert_no_alloc`. `Direct` feeds the
/// port, which `BlockRig` cannot.
///
/// Mutation (run): a `black_box(vec![0.0f32; size])` inside the fed
/// branch of `BrickwallLimiterNode::process` → fails.
#[test]
fn brickwall_limiter_fed_ceiling_process_is_allocation_free() {
    use tutti_graph::contract::Direct;

    let mut node = Direct::new(
        BrickwallLimiterNode::with_channels(ChannelLayout::from(6u16), -0.3),
        SampleRate(48_000.0),
        64,
    );
    for c in node.inputs_mut() {
        for (i, x) in c.iter_mut().enumerate() {
            *x = if i % 2 == 0 { 1.5 } else { -1.5 };
        }
    }
    let ceiling: Vec<f32> = (0..64).map(|i| -12.0 + 0.1875 * i as f32).collect();
    node.feed(0, Some(&ceiling));
    node.block();
    let first = tutti_core::Db(-12.0).to_amplitude().get();
    assert!(
        (node.output(5)[0] - first).abs() < 1e-5,
        "the fed ceiling did not reach the clip: {}",
        node.output(5)[0]
    );
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            node.block();
        }
    });
}

/// A lookahead limiter cloned mid-release, and the clone processed on
/// through the rest of the release under `assert_no_alloc`, with no
/// `prepare` between (which would rebuild the deque at full capacity and
/// hide a short one). At the clone the window-minimum deque holds about
/// one block of a rising run, fewer entries than its window; the clone must
/// still hold the whole window's allocation, or the release grows it on
/// the audio thread.
///
/// Mutation (run): derive `Clone` for `MonotonicMinDeque` (the copy is
/// sized to the entries it holds) → the clone's deque grows inside the
/// gate → fails.
#[test]
fn a_limiter_cloned_mid_release_processes_allocation_free() {
    use tutti_graph::contract::Direct;

    let node = LimiterNode::new(-3.0, -0.3)
        .with_lookahead(0.005)
        .with_release(0.1);
    let mut d = Direct::new(node, SampleRate(48_000.0), 64);
    let fill = |d: &mut Direct<LimiterNode>, amplitude: f32| {
        for c in d.inputs_mut() {
            for (i, x) in c.iter_mut().enumerate() {
                *x = if i % 2 == 0 { amplitude } else { -amplitude };
            }
        }
    };
    // A held burst, long enough (25 release time constants) for the
    // envelope to settle: the gain is then constant, and the deque holds
    // one entry.
    fill(&mut d, 4.0);
    for _ in 0..1_875 {
        d.block();
    }
    // One block into the release: 64 rising gains of a 240-frame window.
    fill(&mut d, 0.05);
    d.block();
    d.node = d.node.clone();
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..200 {
            d.block();
        }
    });
    assert!(
        d.output(0).iter().any(|s| s.abs() > 1e-6),
        "the clone rendered silence after the release"
    );
}
