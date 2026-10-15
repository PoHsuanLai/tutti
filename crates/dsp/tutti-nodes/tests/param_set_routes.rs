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
    Cx, Editor, EventOut, GraphBuilder, Io, Node, ParamIn, ParamNode, ParamRange, ParamSet,
    Prepare, Renderer, Shape, Status, Unforkable,
};
use tutti_mod::{Amount, Clock, ControlProgram, Layer, LayerId, LfoShape, Polarity, Wave};
use tutti_nodes::automation::ControlLaneNode;
use tutti_nodes::{
    CompressorNode, LadderFilterNode, LadderType, LfoNode, ModDelayNode, SvfFilterNode, SvfType,
};
use tutti_types::graph::{OutPort, Source};
use tutti_types::{BeatDuration, Depth, Hz, NodeKey, PhaseIncrement, SampleRate, Samples};

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

/// A chorus, its input noise on both channels, rendered for `frames`;
/// when `lane` is `Some(depth)`, its rate is driven by a control lane
/// whose knob is the chorus's own rate cell and whose one layer is a
/// constant `depth` of the rate port's 0..10 Hz span.
fn render_chorus(
    node: ModDelayNode,
    lane: Option<f32>,
    frames: usize,
) -> (Vec<Vec<f32>>, ParamSet) {
    let mut g = GraphBuilder::new(ChannelLayout::STEREO, ChannelLayout::STEREO);
    let (n, params) = g.add_with_controls(node);
    g.connect_input(0, n, 0)
        .connect_input(1, n, 1)
        .connect_output(n, 0, 0)
        .connect_output(n, 1, 1);
    let at = ParamIn {
        node: n,
        param: UnitParam::Rate,
    };
    g.spec_mut().set_param_range(at, ParamRange::new(0.0, 10.0));
    let mut r = g
        .renderer(Prepare::new(SampleRate(48_000.0), Samples(128)))
        .expect("builds");
    if let Some(depth) = lane {
        let ed = r.editor_mut();
        let layer = Layer {
            id: LayerId(1),
            clock: Clock::Beats {
                per_cycle: BeatDuration(1.0),
                phase: PhaseIncrement(0.0),
            },
            wave: Wave::Const(1.0),
            amount: Amount {
                depth: Depth(depth),
                polarity: Polarity::Bipolar,
                curve: CurveType::Linear,
            },
            depth_by: None,
        };
        let program = ControlProgram::new(None, vec![layer], vec![]).expect("valid");
        let knob = params.cell(UnitParam::Rate).expect("the chorus has a rate");
        let range = ed.param_range_cell(at).expect("committed");
        let lane = ControlLaneNode::for_unit(UnitParam::Rate, program, Some(knob), Some(range));
        ed.insert(NodeKey(9_000), "lane", lane);
        ed.spec_mut().connect_param(
            at,
            EventOut {
                node: NodeKey(9_000),
                port: 0,
            },
        );
        ed.commit().expect("the lane commits");
    }
    let mut state = 0x1234_5678_u32;
    let noise: Vec<f32> = (0..frames)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            0.8 * ((state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0)
        })
        .collect();
    (r.render_input(&[&noise, &noise]), params)
}

/// **A control lane modulates a chorus's rate**, around the chorus's own
/// rate cell: a +2 Hz offset on the 1 Hz knob renders the chorus as it
/// renders at 3 Hz, and not as it does at 1 Hz, and the cell still holds
/// the knob (the lane sends to the port and never writes it, doc 014
/// §4.2). The rate is a port since doc 014 PR 5 ([`MOD_DELAY_PARAMS`]);
/// before, a route onto it was refused with `NoSuchParam` once the
/// frame-rate driver (which wrote the cell) went. The claim the old
/// `chorus_rate_route_moves_the_nodes_own_cell` made, on the lane.
///
/// Mutation (run, PR 5): `ModDelayNode` not declaring its ports (the
/// `.with_params(&MOD_DELAY_PARAMS)` dropped) → the lane's port does not
/// exist and the connect is refused at commit → fails. Mutation (run, PR
/// 5): `LfoDrive::fill_block` ignoring the driven rate → the chorus runs at
/// 1 Hz → fails.
///
/// [`MOD_DELAY_PARAMS`]: tutti_nodes::MOD_DELAY_PARAMS
#[test]
fn a_lane_modulates_a_chorus_rate_around_its_own_cell() {
    const FRAMES: usize = 24_000;
    let chorus = || ModDelayNode::chorus(ChannelLayout::STEREO);
    let (driven, params) = render_chorus(chorus(), Some(0.2), FRAMES);
    let at_3hz = {
        let n = chorus();
        n.set_rate(3.0);
        render_chorus(n, None, FRAMES).0
    };
    let at_1hz = render_chorus(chorus(), None, FRAMES).0;
    for c in 0..2 {
        for i in 0..FRAMES {
            assert!(
                (driven[c][i] - at_3hz[c][i]).abs() < 1e-4,
                "channel {c} frame {i}: driven {} vs 3 Hz {}",
                driven[c][i],
                at_3hz[c][i]
            );
        }
    }
    assert!(
        (0..FRAMES).any(|i| (driven[0][i] - at_1hz[0][i]).abs() > 1e-2),
        "the lane's offset is heard"
    );
    assert_eq!(
        params.get(UnitParam::Rate),
        Some(1.0),
        "the knob is untouched"
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

/// One second of a free-running 1 Hz sine `LfoNode` at depth 0.25, with a
/// control lane on `param` (knob: the LFO's own cell) adding a constant
/// `depth` of `range`, and the cell's value after the render.
fn render_lfo(param: UnitParam, range: ParamRange, depth: f32) -> (Vec<f32>, f32) {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let lfo = LfoNode::new(LfoShape::Sine)
        .with_frequency(Hz(1.0))
        .with_depth(Depth(0.25));
    let (n, params) = g.add_with_controls(lfo);
    g.connect_output(n, 0, 0);
    let at = ParamIn { node: n, param };
    g.spec_mut().set_param_range(at, range);
    let mut r = g
        .renderer(Prepare::new(SampleRate(48_000.0), Samples(128)))
        .expect("builds");
    let knob = params.cell(param).expect("the LFO addresses it");
    let ed = r.editor_mut();
    let layer = Layer {
        id: LayerId(1),
        clock: Clock::Beats {
            per_cycle: BeatDuration(1.0),
            phase: PhaseIncrement(0.0),
        },
        wave: Wave::Const(1.0),
        amount: Amount {
            depth: Depth(depth),
            polarity: Polarity::Bipolar,
            curve: CurveType::Linear,
        },
        depth_by: None,
    };
    let program = ControlProgram::new(None, vec![layer], vec![]).expect("valid");
    let range = ed
        .param_range_cell(at)
        .expect("the LFO declares it as a port");
    let lane = ControlLaneNode::for_unit(param, program, Some(knob.clone()), Some(range));
    ed.insert(NodeKey(9_000), "lane", lane);
    ed.spec_mut().connect_param(
        at,
        EventOut {
            node: NodeKey(9_000),
            port: 0,
        },
    );
    ed.commit().expect("the lane commits");
    let out = r.render(48_000).swap_remove(0);
    (out, knob.load(Ordering::Acquire))
}

/// **A lane drives an `LfoNode`'s depth and rate** (doc 014 PR 5,
/// [`LFO_PARAMS`]), each around the LFO's own cell. The depth knob at 0.25
/// plus half of a 0..1 range swings the 1 Hz sine to ±0.75; the 1 Hz rate
/// knob plus a fifth of a 0..10 Hz range runs it at 3 Hz, three rising
/// zero crossings in a second where 1 Hz has one. Neither cell moves.
///
/// Mutation (run, PR 5): the LFO reading its depth cell, not the driven
/// port (`block_driven(&io, 0)` replaced by `None`) → it peaks at 0.25 →
/// fails. Mutation (run, PR 5): reading its rate cell (`block_driven(&io,
/// 1)` replaced by `None`) → one crossing → fails.
///
/// [`LFO_PARAMS`]: tutti_nodes::LFO_PARAMS
#[test]
fn a_lane_drives_an_lfo_nodes_depth_and_rate() {
    let (out, knob) = render_lfo(UnitParam::Depth, ParamRange::new(0.0, 1.0), 0.5);
    let peak = out.iter().copied().fold(0.0f32, |m, v| m.max(v.abs()));
    assert!((peak - 0.75).abs() < 1e-3, "peaks at 0.75, not {peak}");
    assert_eq!(knob, 0.25, "the lane never writes the depth cell");

    let (out, knob) = render_lfo(UnitParam::Rate, ParamRange::new(0.0, 10.0), 0.2);
    let rising = out.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
    assert_eq!(rising, 3, "3 Hz: three rising crossings in a second");
    assert_eq!(knob, 1.0, "the lane never writes the rate cell");
}
