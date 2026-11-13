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

use assert_no_alloc::AllocDisabler;
use tutti_graph::IntoNode;
use tutti_graph::contract::BlockRig;
use tutti_nodes::{
    BrickwallLimiterNode, CompressorControls, CompressorNode, GateControls, GateNode, KeySource,
    LimiterControls, LimiterNode, Lookahead,
};
use tutti_types::{BusTopology, ChannelLayout, CompressionRatio, Db, SampleRate, Seconds};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// `node` alone in a graph through `BlockRig`: every input an alternating
/// ±`amplitude` (to keep the envelope follower active), `warm` warm-up
/// blocks, then `blocks` 64-frame blocks under `assert_no_alloc`.
fn gate<N: IntoNode>(node: N, amplitude: f32, warm: usize, blocks: usize) {
    let (mut rig, _controls) = BlockRig::new(node, SampleRate::SR_48K, 64);
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
        CompressorNode::mono(CompressorControls {
            threshold: Db::new(-20.0),
            ratio: CompressionRatio::new(4.0),
            attack: Seconds::new(0.005),
            release: Seconds::new(0.050),
            ..CompressorControls::DEFAULT
        }),
        0.7,
        16,
        2_000,
    );
}

#[test]
fn compressor_stereo_process_is_allocation_free() {
    // 4 inputs (L, R, SC-L, SC-R), 2 outputs, linked gain.
    gate(
        CompressorNode::stereo(CompressorControls {
            threshold: Db::new(-18.0),
            knee: Db::new(6.0),
            ratio: CompressionRatio::new(3.0),
            attack: Seconds::new(0.003),
            release: Seconds::new(0.080),
            ..CompressorControls::DEFAULT
        }),
        0.8,
        16,
        2_000,
    );
}

/// The compressor's lane path with every cell moving too: the threshold port fed a per-frame
/// ramp with a NaN frame (the `Feed` lane the recurrence reads), and each block writes
/// the ratio, the times, the knee and the makeup (a new design on the stream grid, the makeup
/// gliding) through the `ParamSet`, 6 channels, 2 000 blocks under `assert_no_alloc`.
#[test]
fn compressor_fed_threshold_and_moving_cells_are_allocation_free() {
    use tutti_graph::ParamNode;
    use tutti_graph::contract::Direct;
    use tutti_types::UnitParam;

    let mut node = Direct::new(
        CompressorNode::new(
            tutti_types::BusTopology::assumed(tutti_types::ChannelLayout::from(6u16))
                .expect("at most one bus wide"),
            KeySource::Sidechain,
            CompressorControls {
                threshold: Db::new(-20.0),
                ratio: CompressionRatio::new(4.0),
                attack: Seconds::new(0.003),
                release: Seconds::new(0.08),
                ..CompressorControls::DEFAULT
            },
        ),
        SampleRate::SR_48K,
        64,
    );
    for c in node.inputs_mut() {
        for (i, x) in c.iter_mut().enumerate() {
            *x = if i % 2 == 0 { 0.8 } else { -0.8 };
        }
    }
    let mut lane: Vec<f32> = (0..64).map(|i| -30.0 + 0.3 * i as f32).collect();
    lane[9] = f32::NAN;
    node.feed(0, Some(&lane));
    let set = node.node.param_set();
    for _ in 0..16 {
        node.block();
    }
    assert_no_alloc::assert_no_alloc(|| {
        for b in 0..2_000 {
            let k = (b % 40) as f32;
            assert!(set.set(UnitParam::Ratio, 2.0 + 0.2 * k));
            assert!(set.set(UnitParam::Attack, 0.0005 + 0.0002 * k));
            assert!(set.set(UnitParam::Release, 0.01 + 0.005 * k));
            assert!(set.set(UnitParam::Knee, 0.3 * k));
            assert!(set.set(UnitParam::GainDb, k - 20.0));
            node.block();
        }
    });
}

#[test]
fn limiter_node_process_is_allocation_free() {
    // Warm-up fills the lookahead ring and primes the deque; over ceiling.
    gate(
        LimiterNode::stereo(LimiterControls {
            threshold: Db::new(-3.0),
            ceiling: Db::new(-0.3),
            ..LimiterControls::DEFAULT
        }),
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
        LimiterNode::new(
            BusTopology::assumed(ChannelLayout::from(6u16)).expect("at most one bus wide"),
            Lookahead::DEFAULT,
            LimiterControls {
                threshold: Db::new(-3.0),
                ceiling: Db::new(-0.3),
                ..LimiterControls::DEFAULT
            },
        ),
        1.5,
        32,
        2_000,
    );
}

#[test]
fn brickwall_limiter_wide_6ch_process_is_allocation_free() {
    gate(
        BrickwallLimiterNode::with_bus(
            BusTopology::unassigned(ChannelLayout::from(6u16)).unwrap(),
            Db::new(-0.3),
        ),
        1.5,
        1,
        5_000,
    );
}

#[test]
fn brickwall_limiter_process_is_allocation_free() {
    gate(BrickwallLimiterNode::new(Db::new(-0.3)), 1.5, 1, 5_000);
}

#[test]
fn gate_mono_process_is_allocation_free() {
    // 2 inputs (audio + sidechain), 1 output.
    gate(
        GateNode::mono(GateControls {
            threshold: Db::new(-40.0),
            attack: Seconds::new(0.001),
            hold: Seconds::new(0.010),
            release: Seconds::new(0.100),
            range: Db::new(-60.0),
        }),
        0.5,
        16,
        2_000,
    );
}

#[test]
fn gate_stereo_process_is_allocation_free() {
    gate(
        GateNode::stereo(GateControls {
            threshold: Db::new(-40.0),
            attack: Seconds::new(0.001),
            hold: Seconds::new(0.010),
            release: Seconds::new(0.100),
            range: Db::new(-80.0),
        }),
        0.6,
        16,
        2_000,
    );
}

/// A lookahead limiter through a burst and its release, every block under
/// `assert_no_alloc` from the first. Whenever the envelope rises (from the
/// node's initial zero onto the burst, and through the release after it)
/// every gain is larger than the last, so the window-minimum deque keeps
/// every push and fills to the whole lookahead window. The constant-drive
/// gates above warm up outside the gate, so the one fill that would grow
/// the deque happens before they measure.
#[test]
fn limiter_node_burst_and_release_are_allocation_free() {
    let node = LimiterNode::stereo(LimiterControls {
        threshold: Db::new(-3.0),
        ceiling: Db::new(-0.3),
        ..LimiterControls::DEFAULT
    });
    let (mut rig, _controls) = BlockRig::new(node, SampleRate::SR_48K, 64);
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

/// The brickwall with its ceiling port fed a per-frame ramp (the `Feed` lane the
/// recurrence reads each frame), 6 channels, 1 000 blocks under `assert_no_alloc`.
/// `Direct` feeds the port, which `BlockRig` cannot.
#[test]
fn brickwall_limiter_fed_ceiling_process_is_allocation_free() {
    use tutti_graph::contract::Direct;

    let mut node = Direct::new(
        BrickwallLimiterNode::with_bus(
            BusTopology::unassigned(ChannelLayout::from(6u16)).unwrap(),
            Db::new(-0.3),
        ),
        SampleRate::SR_48K,
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
    let first = tutti_types::Db(-12.0).to_amplitude().get();
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

/// The brickwall's lane path with the cell moving too, and a lane carrying every
/// value the clip must survive (NaN, `+-inf`, a mute): each block writes the cell to a
/// new ceiling through the `ParamSet` (a new design, retargeted on the audio thread),
/// 2 000 blocks under `assert_no_alloc`.
#[test]
fn brickwall_limiter_moving_cell_and_hostile_lane_are_allocation_free() {
    use tutti_graph::ParamNode;
    use tutti_graph::contract::Direct;
    use tutti_types::UnitParam;

    let mut node = Direct::new(
        BrickwallLimiterNode::with_bus(
            BusTopology::unassigned(ChannelLayout::from(6u16)).unwrap(),
            Db::new(-0.3),
        ),
        SampleRate::SR_48K,
        64,
    );
    for c in node.inputs_mut() {
        for (i, x) in c.iter_mut().enumerate() {
            *x = if i % 2 == 0 { 1.5 } else { -1.5 };
        }
    }
    let mut lane: Vec<f32> = (0..64).map(|i| -12.0 + 0.1875 * i as f32).collect();
    (lane[3], lane[4], lane[5], lane[6]) = (f32::NAN, f32::INFINITY, f32::NEG_INFINITY, f32::NAN);
    node.feed(0, Some(&lane));
    let set = node.node.param_set();
    node.block();
    assert_no_alloc::assert_no_alloc(|| {
        for b in 0..2_000 {
            let db = -20.0 + (b % 40) as f32 * 0.5;
            assert!(set.set(UnitParam::Ceiling, if b % 7 == 0 { f32::NAN } else { db }));
            node.block();
        }
    });
}

/// The lookahead limiter with its ceiling and threshold ports fed per frame
/// (the path that reads both per sample instead of through the block's
/// curve), 6 channels, 1 000 blocks under `assert_no_alloc`, the first
/// measured after a warm-up that has the limiter reducing.
#[test]
fn limiter_node_fed_ceiling_and_threshold_process_is_allocation_free() {
    use tutti_graph::contract::Direct;

    let mut node = Direct::new(
        LimiterNode::new(
            BusTopology::assumed(ChannelLayout::from(6u16)).expect("at most one bus wide"),
            Lookahead::DEFAULT,
            LimiterControls::DEFAULT,
        ),
        SampleRate::SR_48K,
        64,
    );
    for c in node.inputs_mut() {
        for (i, x) in c.iter_mut().enumerate() {
            *x = if i % 2 == 0 { 1.5 } else { -1.5 };
        }
    }
    let ceiling: Vec<f32> = (0..64).map(|i| -6.0 + 0.0625 * i as f32).collect();
    let threshold: Vec<f32> = (0..64).map(|i| -12.0 + 0.1 * i as f32).collect();
    // The ports are in the set's order: threshold, ceiling.
    node.feed(0, Some(&threshold));
    node.feed(1, Some(&ceiling));
    for _ in 0..32 {
        node.block();
    }
    assert!(
        node.output(0).iter().any(|y| y.abs() < 1.0),
        "the limiter was not reducing; the gate would walk no gain curve"
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
#[test]
fn a_limiter_cloned_mid_release_processes_allocation_free() {
    use tutti_graph::contract::Direct;

    let node = LimiterNode::stereo(LimiterControls {
        threshold: Db::new(-3.0),
        ceiling: Db::new(-0.3),
        ..LimiterControls::DEFAULT
    });
    let mut d = Direct::new(node, SampleRate::SR_48K, 64);
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
