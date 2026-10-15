//! A node's [`ParamSet`](tutti_graph::ParamSet) is the address a modulation
//! host resolves a param on: `ParamSet::cell(p)` is the very cell the node
//! reads, which an engine hands a control lane as its knob (doc 014 §4.2:
//! one writer per knob, the host's `set`), so a write through the set moves
//! the node's own control and the lane's offsets ride on it.
//!
//! The `ParamSet` is each node's one address: a set is addressed by
//! `UnitParam` alone, and each cell has one address (the compressor's makeup
//! is `GainDb`).
//!
//! Ported in doc 014 PR 5 from routes through a `ModMatrix` onto an
//! `AtomicTarget` mirroring the cell (both deleted, §10): the address checks
//! now write through `ParamSet::set`, and the end-to-end check drives a
//! control lane whose knob is the filter's cell.
//!
//! Mutation (run): `ParamSet::cell` answering every param with the set's
//! first cell → every test asking for a param the set lacks, or one that is
//! not its first, fails (5 of 7, with `convolution`); the two that address
//! the SVF's cutoff, its first param, pass, as they must.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use audio_automation::CurveType;
use tutti_core::{ChannelLayout, UnitParam};
use tutti_graph::{
    Cx, Editor, EventOut, Io, Node, ParamIn, ParamNode, ParamRange, Prepare, Renderer, Shape,
    Status, Unforkable,
};
use tutti_mod::{Amount, Clock, ControlProgram, Layer, LayerId, LfoShape, Polarity, Wave};
use tutti_nodes::automation::ControlLaneNode;
use tutti_nodes::{
    CompressorNode, LadderFilterNode, LadderType, ModDelayNode, SvfFilterNode, SvfType,
};
use tutti_types::graph::{OutPort, Source};
use tutti_types::{Depth, Hz, NodeKey, PhaseIncrement, SampleRate, Samples};

/// Writes `value` to `p` through `node`'s set, as a host does; `false` for
/// a param the set lacks.
fn set<N: ParamNode>(node: &N, p: UnitParam, value: f32) -> bool {
    node.param_set().set(p, value)
}

/// Whether `node`'s set addresses `p` (has a cell a lane could take).
fn addresses<N: ParamNode>(node: &N, p: UnitParam) -> bool {
    node.param_set().cell(p).is_some()
}

fn svf() -> SvfFilterNode<f32> {
    SvfFilterNode::<f32>::with_channels(ChannelLayout::STEREO, SvfType::LowPass, 1000.0, 0.7)
}

#[test]
fn filter_cutoff_write_moves_the_nodes_own_cell() {
    let node = svf();
    let atomic = node.frequency(); // the node reads this per block
    assert!(set(&node, UnitParam::Cutoff, 1500.0), "cutoff is addressed");
    assert_eq!(atomic.load(Ordering::Acquire), 1500.0);
    // The cell a lane takes as its knob is that same cell.
    let cell = node.param_set().cell(UnitParam::Cutoff).expect("addressed");
    assert!(Arc::ptr_eq(&cell, &atomic));
}

#[test]
fn a_param_the_set_lacks_has_no_address() {
    // The SVF has no Drive param.
    assert!(!addresses(&svf(), UnitParam::Drive));
    assert!(!set(&svf(), UnitParam::Drive, 1.0));
}

#[test]
fn ladder_addresses_q_and_drive_but_not_feedback() {
    let node =
        LadderFilterNode::<f32>::with_channels(ChannelLayout::STEREO, LadderType::LP24, 800.0, 0.5);
    assert!(addresses(&node, UnitParam::Cutoff));
    assert!(addresses(&node, UnitParam::Q));
    assert!(addresses(&node, UnitParam::Drive));
    assert!(!addresses(&node, UnitParam::Feedback));
}

// ── End-to-end: a lane whose knob is a REAL filter's cell ──

/// Writes its cutoff port's value per frame on its one output.
struct Probe;

impl Node for Probe {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO).with_params(&[UnitParam::Cutoff])
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let v: Option<Vec<f32>> = io.param(0).frames().map(<[f32]>::to_vec);
        match v {
            Some(v) => io.output(0).copy_from_slice(&v),
            None => io.output(0).fill(0.0),
        }
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, _: usize) -> Option<f32> {
        Some(0.0)
    }
}

/// A sine LFO lane, its knob the filter's own cutoff cell, sweeps a cutoff
/// port around that cell's value, both ways and inside the range; a write
/// through the filter's set moves the centre of the sweep on the next
/// block, so the knob the lane reads is the node's cell, not a copy.
///
/// Mutation (run): build the lane's knob from a snapshot of the cell
/// (`Arc::new(AtomicF32::new(cell.load(..)))`) → the second sweep stays
/// centred on 1000 → fails.
#[test]
fn an_lfo_lane_sweeps_around_a_real_filters_cutoff_cell() {
    let filter = svf();
    let (lo, hi) = (500.0, 1500.0);
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(64)));
    let probe = NodeKey(1);
    ed.insert(probe, "probe", Unforkable(Probe));
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort {
        node: probe,
        port: 0,
    })];
    let at = ParamIn {
        node: probe,
        param: UnitParam::Cutoff,
    };
    ed.set_param_range(at, ParamRange::new(lo, hi))
        .expect("a probe");
    ed.commit().expect("commits");
    let layer = Layer {
        id: LayerId(1),
        // 10 cycles a second: a cycle every 4800 frames.
        clock: Clock::Seconds {
            rate: Hz(10.0),
            phase: PhaseIncrement(0.0),
            anchor: None,
        },
        wave: Wave::Shape {
            shape: LfoShape::Sine,
            seed: 0,
        },
        amount: Amount {
            depth: Depth(0.25),
            polarity: Polarity::Bipolar,
            curve: CurveType::Linear,
        },
        depth_by: None,
    };
    let program = ControlProgram::new(None, vec![layer], vec![]).expect("valid");
    let knob = filter
        .param_set()
        .cell(UnitParam::Cutoff)
        .expect("addressed");
    let range = ed.param_range_cell(at).expect("committed");
    let lane = ControlLaneNode::for_unit(UnitParam::Cutoff, program, Some(knob), Some(range));
    ed.insert(NodeKey(2), "lane", lane);
    ed.spec_mut().connect_param(
        at,
        EventOut {
            node: NodeKey(2),
            port: 0,
        },
    );
    ed.commit().expect("the lane commits");

    let mut r = Renderer::new(ed, exec);
    r.set_block(Samples(64));
    // One cycle and a block.
    let mut sweep = || r.render(4800 + 64).swap_remove(0);
    let first = sweep();
    let (min, max) = first
        .iter()
        .fold((f32::MAX, f32::MIN), |(a, b), &v| (a.min(v), b.max(v)));
    assert!(
        (lo..=hi).contains(&min) && (lo..=hi).contains(&max),
        "{min}..{max}"
    );
    assert!(
        max > 1200.0 && min < 800.0,
        "sweeps both ways: {min}..{max}"
    );

    assert!(set(&filter, UnitParam::Cutoff, 1200.0));
    let second = sweep();
    let max = second.iter().copied().fold(f32::MIN, f32::max);
    assert!(
        max > 1400.0,
        "the sweep follows the filter's cell: max {max}"
    );
}

#[test]
fn chorus_rate_write_moves_the_nodes_own_cell() {
    let node = ModDelayNode::chorus(ChannelLayout::STEREO);
    let rate_atomic = node.rate();
    let base = rate_atomic.load(Ordering::Acquire);
    assert!(
        set(&node, UnitParam::Rate, base + 2.0),
        "chorus rate is addressed"
    );
    assert_eq!(rate_atomic.load(Ordering::Acquire), base + 2.0);

    // A param it doesn't expose has no address.
    assert!(!addresses(&node, UnitParam::Cutoff));
}

/// The compressor's makeup has one address, `GainDb`; `Makeup` is not in
/// its set.
#[test]
fn compressor_makeup_is_addressed_as_gain_db() {
    let node = CompressorNode::with_channels(
        tutti_core::Db(-12.0),
        tutti_core::CompressionRatio(4.0),
        tutti_core::Seconds(0.01),
        tutti_core::Seconds(0.1),
        2,
    );
    let makeup = node.makeup_gain();
    assert!(set(&node, UnitParam::GainDb, 6.0), "makeup is GainDb");
    assert_eq!(makeup.load(Ordering::Acquire), 6.0);
    assert!(!addresses(&node, UnitParam::Makeup));
}

#[cfg(feature = "convolution")]
#[test]
fn convolver_only_addresses_wet() {
    // Room size is baked into the IR; only Wet is a control.
    // A minimal unit IR is fine — only the param surface is probed.
    let node = tutti_nodes::ConvolverNode::shared_ir(2usize, &[1.0], 64);
    assert!(addresses(&node, UnitParam::Wet));
    assert!(!addresses(&node, UnitParam::RoomSize));
    assert!(!addresses(&node, UnitParam::Cutoff));
}
