//! Regression gate: modulation + delay nodes must not allocate per-buffer.
//!
//! Covers `LfoNode` (free-running and beat-synced), `DelayLineNode` (mono,
//! stereo cross-fed, 6-wide), and the modulation effects `ModDelayNode`
//! (chorus and flanger) and `PhaserNode`. They are graph nodes, each
//! driven alone in a graph by `tutti_graph::contract::BlockRig`: its `prepare`
//! (which sizes the delay lines and the block scratch) runs when the rig is
//! built, outside the gate; the gate walks only `process`.

use assert_no_alloc::AllocDisabler;
use tutti_core::SampleRate;
use tutti_graph::contract::BlockRig;
use tutti_graph::IntoNode;
use tutti_nodes::{DelayLineNode, LfoNode, LfoShape, ModDelayNode, PhaserNode};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// A node through `BlockRig`: its inputs an alternating signal of
/// `amplitude`, 32 warm-up blocks, then `blocks` blocks under
/// `assert_no_alloc`, `between(i)` run before each (a control move).
fn gate<N: IntoNode>(node: N, amplitude: f32, blocks: usize, mut between: impl FnMut(usize)) {
    let (mut rig, _controls) = BlockRig::new(node, SampleRate(48_000.0), 64);
    for c in rig.inputs_mut() {
        for (i, x) in c.iter_mut().enumerate() {
            *x = if i % 2 == 0 { amplitude } else { -amplitude };
        }
    }
    for i in 0..32 {
        between(i);
        rig.block();
    }
    assert!(
        rig.output(0).iter().any(|s| s.abs() > 1e-6),
        "the node rendered silence; the gate would walk no DSP"
    );
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..blocks {
            between(i);
            rig.block();
        }
    });
}

#[test]
fn lfo_node_process_is_allocation_free() {
    gate(
        LfoNode::new(LfoShape::Sine).with_frequency(2.0_f32),
        0.0,
        5_000,
        |_| {},
    );
}

#[test]
fn lfo_random_shape_process_is_allocation_free() {
    // RandomSmooth path has its own per-instance state machine. Fast enough
    // to step inside the warm-up, so the node is audible there.
    gate(
        LfoNode::new(LfoShape::RandomSmooth).with_frequency(200.0_f32),
        0.0,
        5_000,
        |_| {},
    );
}

/// The beat-synced path walks the block's `Env` frame by frame
/// (`Env::for_each_beat`); a quarter-cycle offset puts the stopped
/// transport's beat 0 on the sine's peak, so the node is audible.
///
/// Mutation (run): a `Vec::with_capacity(size)` scratch in the
/// `BeatSynced` branch of `ModulatorNode::process` fails the gate.
#[test]
fn beat_synced_lfo_process_is_allocation_free() {
    gate(
        LfoNode::new(LfoShape::Sine)
            .with_beat_sync(4.0)
            .with_phase_offset(0.25),
        0.0,
        5_000,
        |_| {},
    );
}

#[test]
fn delay_line_node_process_is_allocation_free() {
    // Half dry, so the warm-up (shorter than the 250 ms tap) is audible.
    let node = DelayLineNode::new(2.0_f32, 0.25_f32, 0.4_f32);
    node.set_mix(0.5_f32);
    gate(node, 0.5, 2_000, |_| {});
}

#[test]
fn stereo_delay_line_node_process_is_allocation_free() {
    let node = DelayLineNode::stereo(2.0_f32, 0.30_f32, 0.31_f32, 0.4_f32);
    node.set_mix(0.5_f32);
    gate(node, 0.5, 2_000, |_| {});
}

#[test]
fn stereo_delay_line_node_wide_6ch_process_is_allocation_free() {
    // The per-channel delay-line Vec must be built at construction; the wide
    // path must not allocate per buffer.
    let node = DelayLineNode::with_channels(6usize, 2.0_f32, 0.30_f32, 0.4_f32);
    node.set_mix(0.5_f32);
    gate(node, 0.5, 2_000, |_| {});
}

#[test]
fn chorus_node_process_is_allocation_free() {
    gate(
        ModDelayNode::chorus(tutti_core::ChannelLayout::STEREO),
        0.4,
        2_000,
        |_| {},
    );
}

#[test]
fn flanger_node_process_is_allocation_free() {
    gate(
        ModDelayNode::flanger(tutti_core::ChannelLayout::STEREO),
        0.4,
        2_000,
        |_| {},
    );
}

#[test]
fn phaser_node_process_is_allocation_free() {
    // 4-stage phaser. `prepare` allocates the scratch, but `process` must
    // reuse it.
    gate(PhaserNode::new(4), 0.4, 2_000, |_| {});
}

/// Every control moved each block: the delay time glides, the mixes fade,
/// the phaser's coefficient table is re-solved, the cross-fed delay reads
/// every tap before writing any line. All of it on preallocated scratch.
/// The moves go through clones of the nodes, which share their cells.
///
/// Mutation (run): a `Vec::with_capacity(w)` scratch in the phaser's
/// `render` fails.
#[test]
fn delay_and_modulation_with_moving_controls_are_allocation_free() {
    let delay = DelayLineNode::stereo(1.0_f32, 0.2_f32, 0.3_f32, 0.4_f32);
    delay.set_cross_feedback(0.3);
    let d = delay.clone();
    gate(delay, 0.5, 2_000, |i| {
        let t = (i % 50) as f32 / 50.0;
        d.set_delay_time(0.05 + 0.4 * t);
        d.set_mix(t);
    });
    let chorus = ModDelayNode::chorus(6usize);
    let c = chorus.clone();
    gate(chorus, 0.5, 2_000, |i| {
        let t = (i % 50) as f32 / 50.0;
        c.set_depth(0.002 + 0.01 * t);
        c.set_mix(1.0 - t);
    });
    let phaser = PhaserNode::with_channels(6usize, 8);
    let p = phaser.clone();
    gate(phaser, 0.5, 2_000, |i| {
        let t = (i % 50) as f32 / 50.0;
        p.set_depth(t);
        p.set_feedback(0.9 * t);
    });
}
