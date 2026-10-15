//! Regression gate: modulation + delay nodes must not allocate per-buffer.
//!
//! Covers `LfoNode` (free-running and beat-synced), `DelayLineNode` (mono,
//! stereo cross-fed, 6-wide), and the modulation effects `ModDelayNode`
//! (chorus and flanger) and `PhaserNode`. They are graph nodes, each
//! driven alone in a graph by `tutti_graph::contract::BlockRig`: its `prepare`
//! (which sizes the delay lines and the block scratch) runs when the rig is
//! built, outside the gate; the gate walks only `process`. And control
//! lanes (`ControlLaneNode`, `AutomationLaneNode::for_param`'s too) driving
//! param ports through a graph, serial and on a pool: four automation
//! lanes, and 200 lanes of every layer kind with republishes, knob writes
//! and range edits between blocks.

use assert_no_alloc::AllocDisabler;
use tutti_core::{ChannelLayout, SampleRate};
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

// ---- an event-output lane on the audio thread ---------------------------------

/// Writes the `Cutoff` it reads (base 0) per frame, allocation-free.
struct ParamProbe;

impl tutti_graph::Node for ParamProbe {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
            .with_params(&[tutti_types::UnitParam::Cutoff])
    }
    fn prepare(&mut self, _: &tutti_graph::Prepare) {}
    fn process(
        &mut self,
        _: &tutti_graph::Cx<'_>,
        mut io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        for i in 0..io.frames() {
            let v = io.param(0).frames().map_or(0.0, |f| f[i]);
            io.output(0)[i] = v;
        }
        tutti_graph::Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, port: usize) -> Option<f32> {
        (port == 0).then_some(0.0)
    }
}

/// A sine of the beat: a curve with no pieces (the lane's fallback).
struct SineCurve;

impl tutti_nodes::automation::Curve for SineCurve {
    fn value_at(&self, beat: tutti_types::Beat) -> Option<f32> {
        Some((beat.get() as f32 * 7.0).sin())
    }
}

/// Four event-output lanes, each driving its own probe's cutoff (outputs
/// 0..4): exponential, cubic-out and S-curve pieces, an `Elastic` piece
/// (the linear fallback), and a curve with no pieces.
fn lane_graph(max: usize) -> (tutti_graph::Editor, tutti_graph::Executor) {
    use tutti_graph::{EventOut, ParamFrom, ParamIn, ParamShaping, Prepare, Unforkable};
    use tutti_nodes::automation::{
        AutomationEnvelope, AutomationLaneNode, AutomationPoint, CurveType,
    };
    use tutti_types::graph::{OutPort, Source};
    use tutti_types::{Hz, NodeKey, ParamKey, Samples, UnitParam};
    let mut shaped = AutomationEnvelope::new(());
    shaped.add_point(AutomationPoint::with_curve(
        0.0,
        0.0,
        CurveType::Exponential,
    ));
    shaped.add_point(AutomationPoint::with_curve(0.25, 1.0, CurveType::CubicOut));
    shaped.add_point(AutomationPoint::with_curve(0.4, 0.2, CurveType::SCurve));
    shaped.add_point(AutomationPoint::new(0.55, 0.9));
    let mut elastic = AutomationEnvelope::new(());
    elastic.add_point(AutomationPoint::with_curve(0.1, 0.0, CurveType::Elastic));
    elastic.add_point(AutomationPoint::new(0.45, 1.0));
    let key = ParamKey::<Hz>::CUTOFF;
    let lanes = [
        AutomationLaneNode::for_param(shaped.clone(), key),
        AutomationLaneNode::for_param(elastic, key),
        AutomationLaneNode::for_param(SineCurve, key),
        AutomationLaneNode::for_param(shaped, key),
    ];
    let (mut ed, exec) = tutti_graph::Editor::new(Prepare::new(SampleRate(48_000.0), Samples(max)));
    let mut outputs = Vec::new();
    for (k, lane) in lanes.into_iter().enumerate() {
        let (l, p) = (NodeKey(10 + k as u64), NodeKey(20 + k as u64));
        ed.insert(l, "lane", lane);
        ed.insert(p, "probe", Unforkable(ParamProbe));
        ed.spec_mut().connect_param(
            ParamIn {
                node: p,
                param: UnitParam::Cutoff,
            },
            ParamFrom::Events(EventOut { node: l, port: 0 }),
            ParamShaping::Identity,
        );
        outputs.push(Source::Node(OutPort { node: p, port: 0 }));
    }
    ed.spec_mut().topology.outputs = outputs;
    ed.commit().expect("commits");
    (ed, exec)
}

/// The block schedule the lane rigs render: ragged, up to 128 frames.
fn lane_blocks(total: usize) -> Vec<usize> {
    let mut v = Vec::new();
    let (mut sum, mut i) = (0, 0usize);
    while sum < total {
        let n = [1, 128, 7, 64, 127, 33, 2, 100][i % 8].min(total - sum);
        v.push(n);
        sum += n;
        i += 1;
    }
    v
}

/// One block of the lane graph at the host's clock `clock` (113 BPM,
/// looping 0.1 to 0.5 beats, so wraps land inside blocks), the clock moved
/// on; the four outputs appended to `out`.
fn lane_block(
    exec: &mut tutti_graph::Executor,
    clock: &mut tutti_types::FrameClock,
    n: usize,
    bufs: &mut [Vec<f32>; 4],
) {
    let looping = tutti_types::LoopRange::new(tutti_types::Beat(0.1), tutti_types::Beat(0.5));
    let graph_loop = looping.map(|l| tutti_graph::LoopRange {
        start: l.start(),
        end: l.end(),
    });
    let t = tutti_graph::Transport::counted(true, clock.tempo(), clock.origin(), graph_loop);
    let [a, b, c, d] = bufs;
    exec.process(
        n,
        &t,
        &[],
        &mut [&mut a[..n], &mut b[..n], &mut c[..n], &mut d[..n]],
    );
    clock.advance(tutti_types::Samples(n), looping);
}

/// A [`WorkerPool`](tutti_core::WorkerPool) whose every participant runs its
/// share of a job inside `assert_no_alloc`, the helper threads included
/// (the gate is per thread).
struct GatedPool(tutti_core::WorkerPool);

impl tutti_core::Pool for GatedPool {
    fn participants(&self) -> usize {
        self.0.participants()
    }
    fn run(&self, wake: usize, job: &(dyn Fn(usize) + Sync)) {
        self.0
            .run(wake, &|i| assert_no_alloc::assert_no_alloc(|| job(i)));
    }
}

/// An automation lane's event output (`AutomationLaneNode::for_param`)
/// wired to a param port never allocates on the audio thread: curve
/// segments at breakpoints, loop wraps inside blocks, the linear fallback
/// for an `Elastic` piece and for a curve with no pieces, ragged blocks;
/// serial, and on a pool whose helpers run under the gate too.
///
/// Changed in doc 014 PR 2b: the lanes are `ControlLaneNode`s, and the
/// `Elastic` and sine lanes send grid points of the play clock; the
/// no-allocation property stands.
///
/// Mutation (run): a `Vec::with_capacity(8)` per block in
/// `ControlLaneNode::process` → both gates abort (and the 200-lane gate's).
#[test]
fn event_lanes_are_allocation_free_serial_and_on_a_pool() {
    use std::sync::Arc;
    for workers in [None, Some(4)] {
        let (mut ed, mut exec) = lane_graph(128);
        if let Some(w) = workers {
            let pool = tutti_core::WorkerPool::builder(w)
                .spin(std::time::Duration::from_micros(1))
                .idle(std::time::Duration::from_micros(1))
                .build();
            exec.set_pool(Some(Arc::new(GatedPool(pool))));
        }
        let mut clock = tutti_types::FrameClock::new(
            tutti_types::Beat(0.0),
            tutti_types::Bpm(113.0),
            SampleRate(48_000.0),
        );
        let mut bufs: [Vec<f32>; 4] = std::array::from_fn(|_| vec![0.0; 128]);
        for n in lane_blocks(4_000) {
            lane_block(&mut exec, &mut clock, n, &mut bufs);
            ed.collect();
        }
        assert_eq!(exec.is_parallel(), workers.is_some(), "{workers:?}");
        assert!(
            bufs.iter().all(|b| b.iter().any(|v| v.abs() > 1e-4)),
            "every lane moved its param: the gate walks the segment path"
        );
        let blocks = lane_blocks(30_000);
        assert_no_alloc::assert_no_alloc(|| {
            for &n in &blocks {
                lane_block(&mut exec, &mut clock, n, &mut bufs);
            }
        });
        ed.collect();
    }
}

/// The lanes render bit for bit the same serial and on pools of 2, 3, 4
/// and 8 participants, under ragged blocks with loop wraps inside them.
///
/// Not mutation-tested against a property of the lane: a lane's state is
/// its own (one node, one op), so nothing it does can differ between the
/// two; this pins that the event path keeps it that way (a lane sharing
/// state with another, which `CLAUDE.md` forbids, would diverge here).
#[test]
fn event_lanes_render_the_same_serial_and_parallel() {
    use std::sync::Arc;
    let render = |workers: Option<usize>| {
        let (mut ed, mut exec) = lane_graph(128);
        if let Some(w) = workers {
            exec.set_pool(Some(Arc::new(tutti_core::WorkerPool::builder(w).build())));
        }
        let mut clock = tutti_types::FrameClock::new(
            tutti_types::Beat(0.0),
            tutti_types::Bpm(113.0),
            SampleRate(48_000.0),
        );
        let mut bufs: [Vec<f32>; 4] = std::array::from_fn(|_| vec![0.0; 128]);
        let mut all: [Vec<u32>; 4] = Default::default();
        for n in lane_blocks(20_000) {
            lane_block(&mut exec, &mut clock, n, &mut bufs);
            ed.collect();
            for (a, b) in all.iter_mut().zip(&bufs) {
                a.extend(b[..n].iter().map(|v| v.to_bits()));
            }
        }
        all
    };
    let serial = render(None);
    for w in [2, 3, 4, 8] {
        assert!(render(Some(w)) == serial, "{w} participants");
    }
}

// ---- 200 control lanes ----------------------------------------------------

/// The eight params a [`Probe8`] declares, one per lane of a group.
const PROBE8: [tutti_types::UnitParam; 8] = {
    use tutti_types::UnitParam::*;
    [Cutoff, Q, GainDb, Wet, Feedback, Depth, Rate, Threshold]
};

/// Declares the eight [`PROBE8`] params (base 0) and writes each port's
/// value to its own output, allocation-free.
struct Probe8;

impl tutti_graph::Node for Probe8 {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::audio(ChannelLayout::EMPTY, ChannelLayout::from_count(8))
            .with_params(&PROBE8)
    }
    fn prepare(&mut self, _: &tutti_graph::Prepare) {}
    fn process(
        &mut self,
        _: &tutti_graph::Cx<'_>,
        mut io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        for p in 0..PROBE8.len() {
            for i in 0..io.frames() {
                let v = io.param(p).frames().map_or(0.0, |f| f[i]);
                io.output(p)[i] = v;
            }
        }
        tutti_graph::Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, port: usize) -> Option<f32> {
        (port < PROBE8.len()).then_some(0.0)
    }
}

/// Lane `k`'s program in generation `gen` (a republish moves its depths):
/// five kinds in turn, beats, seconds, drawn, random, and automation under
/// a layer.
fn lane_program(k: usize, gen: u32) -> tutti_mod::ControlProgram {
    use std::sync::Arc;
    use tutti_mod::{
        Amount, Clock, ControlProgram, CurveType, Layer, LayerId, LfoShape, PeriodPoint, Polarity,
        Wave,
    };
    use tutti_nodes::automation::{AutomationEnvelope, AutomationPoint};
    use tutti_types::{BeatDuration, Depth, Hz, Phase, PhaseIncrement};
    let depth = 0.05 + 0.01 * ((k as u32 + gen) % 7) as f32;
    let amount = Amount {
        depth: Depth(depth),
        polarity: Polarity::Bipolar,
        curve: if k.is_multiple_of(3) {
            CurveType::Exponential
        } else {
            CurveType::Linear
        },
    };
    let beats = Clock::Beats {
        per_cycle: BeatDuration(0.25 + 0.125 * (k % 5) as f64),
        phase: PhaseIncrement(0.0),
    };
    let seconds = Clock::Seconds {
        rate: Hz(1.0 + (k % 11) as f32),
        phase: PhaseIncrement(0.0),
        anchor: None,
    };
    let shape = |shape| Wave::Shape {
        shape,
        seed: k as u64,
    };
    let layer = |clock, wave| Layer {
        id: LayerId(1),
        clock,
        wave,
        amount,
        depth_by: None,
    };
    let (automation, layers) = match k % 5 {
        0 => (None, vec![layer(beats, shape(LfoShape::Sine))]),
        1 => (None, vec![layer(seconds, shape(LfoShape::Triangle))]),
        2 => (
            None,
            vec![layer(
                beats,
                Wave::Drawn(Arc::from(vec![
                    PeriodPoint {
                        at: Phase(0.0),
                        value: -0.5,
                        curve: CurveType::Linear,
                    },
                    PeriodPoint {
                        at: Phase(0.4),
                        value: 1.0,
                        curve: CurveType::SCurve,
                    },
                ])),
            )],
        ),
        3 => (None, vec![layer(seconds, shape(LfoShape::RandomSmooth))]),
        _ => {
            let mut e = AutomationEnvelope::new(());
            e.add_point(AutomationPoint::with_curve(
                0.0,
                200.0,
                CurveType::Exponential,
            ));
            e.add_point(AutomationPoint::new(1.3, 800.0));
            let curve: Arc<dyn tutti_mod::Curve> = Arc::new(e);
            (Some(curve), vec![layer(seconds, shape(LfoShape::Sawtooth))])
        }
    };
    ControlProgram::new(automation, layers, vec![]).expect("a valid program")
}

/// Lane `k` for param `k % 8` of its probe, built against that port's
/// range cell, with a knob of its own.
fn lane_for(
    k: usize,
    range: tutti_graph::ParamRangeReader,
) -> tutti_nodes::automation::ControlLaneNode {
    use std::sync::Arc;
    use tutti_nodes::automation::ControlLaneNode as L;
    use tutti_types::{AtomicF32, Db, Depth, Feedback, Hz, Mix, ParamKey, Q};
    let p = lane_program(k, 0);
    let knob = Some(Arc::new(AtomicF32::new(400.0 + k as f32)));
    let r = Some(range);
    match k % 8 {
        0 => L::for_param(ParamKey::<Hz>::CUTOFF, p, knob, r),
        1 => L::for_param(ParamKey::<Q>::Q, p, knob, r),
        2 => L::for_param(ParamKey::<Db>::GAIN_DB, p, knob, r),
        3 => L::for_param(ParamKey::<Mix>::WET, p, knob, r),
        4 => L::for_param(ParamKey::<Feedback>::FEEDBACK, p, knob, r),
        5 => L::for_param(ParamKey::<Depth>::DEPTH, p, knob, r),
        6 => L::for_param(ParamKey::<Hz>::RATE, p, knob, r),
        _ => L::for_param(ParamKey::<Db>::THRESHOLD, p, knob, r),
    }
}

/// 200 lanes into the params of 25 [`Probe8`]s (one lane per port: a port
/// takes one event source), ranged 0..1000; the 200 outputs in lane order.
fn lanes_200(
    max: usize,
) -> (
    tutti_graph::Editor,
    tutti_graph::Executor,
    Vec<tutti_nodes::automation::LaneControls>,
) {
    use tutti_graph::{
        EventOut, ParamFrom, ParamIn, ParamRange, ParamShaping, Prepare, Unforkable,
    };
    use tutti_types::graph::{OutPort, Source};
    use tutti_types::{NodeKey, Samples};
    let (mut ed, exec) = tutti_graph::Editor::new(Prepare::new(SampleRate(48_000.0), Samples(max)));
    let at = |k: usize| ParamIn {
        node: NodeKey(1000 + (k / 8) as u64),
        param: PROBE8[k % 8],
    };
    for j in 0..25u64 {
        ed.insert(NodeKey(1000 + j), "probe", Unforkable(Probe8));
        for p in 0..8 {
            ed.spec_mut().topology.outputs.push(Source::Node(OutPort {
                node: NodeKey(1000 + j),
                port: p,
            }));
        }
    }
    for k in 0..200 {
        ed.set_param_range(at(k), ParamRange::new(0.0, 1000.0))
            .expect("a probe");
    }
    ed.commit().expect("commits");
    let mut controls = Vec::new();
    for k in 0..200 {
        let lane = lane_for(k, ed.param_range_cell(at(k)).expect("committed"));
        let key = NodeKey(2000 + k as u64);
        controls.push(ed.insert(key, "lane", lane));
        ed.spec_mut().connect_param(
            at(k),
            ParamFrom::Events(EventOut { node: key, port: 0 }),
            ParamShaping::Identity,
        );
    }
    ed.commit().expect("commits");
    (ed, exec, controls)
}

/// The control-thread moves between blocks, the same in every render:
/// every 10th block a republish of every program (new depths), every block
/// a knob write on every 7th lane, every 15th block a range edit on every
/// 9th port.
fn lane_moves(
    ed: &mut tutti_graph::Editor,
    controls: &[tutti_nodes::automation::LaneControls],
    block: usize,
) {
    use tutti_graph::{ParamIn, ParamRange};
    use tutti_types::NodeKey;
    if block % 10 == 9 {
        for (k, c) in controls.iter().enumerate() {
            c.publish(lane_program(k, block as u32));
        }
    }
    for (k, c) in controls.iter().enumerate().skip(block % 7).step_by(7) {
        c.set_knob(300.0 + ((k + block) % 13) as f32 * 20.0);
    }
    if block % 15 == 14 {
        for k in (block % 9..200).step_by(9) {
            let at = ParamIn {
                node: NodeKey(1000 + (k / 8) as u64),
                param: PROBE8[k % 8],
            };
            let hi = 800.0 + ((k + block) % 5) as f32 * 100.0;
            ed.set_param_range(at, ParamRange::new(0.0, hi))
                .expect("a probe");
        }
    }
}

/// The 120 BPM transport the 200-lane rigs roll, looping 0.5 to 1.7 beats
/// (wraps inside blocks).
fn lanes_transport(clock: &tutti_types::FrameClock) -> tutti_graph::Transport {
    tutti_graph::Transport::counted(
        true,
        clock.tempo(),
        clock.origin(),
        Some(tutti_graph::LoopRange {
            start: tutti_types::Beat(0.5),
            end: tutti_types::Beat(1.7),
        }),
    )
}

fn lanes_loop() -> Option<tutti_types::LoopRange> {
    tutti_types::LoopRange::new(tutti_types::Beat(0.5), tutti_types::Beat(1.7))
}

/// 200 control lanes (doc 014 §12) render bit for bit the same serial and
/// on pools of 2, 3, 4 and 8 participants, under ragged blocks with loop
/// wraps inside them, republishes of every program, knob writes and range
/// edits between blocks.
///
/// Not mutation-tested against a property of the lane: each lane's state
/// is its own (one node, one op, its cells read once per block), so nothing
/// it does can differ between schedules. This pins that it stays so: two
/// lanes sharing mutable state outside the graph (`CLAUDE.md` forbids it)
/// would diverge here. Mutation (run): state shared across lanes (each
/// lane's knob offset by 0.5 on the parity of one `static` counter every
/// lane's `process` bumps) → the pools run the lanes in another order and
/// render other bits than serial → fails.
#[test]
fn two_hundred_lanes_render_the_same_serial_and_parallel() {
    use std::sync::Arc;
    let render = |workers: Option<usize>| {
        let (mut ed, mut exec, controls) = lanes_200(128);
        if let Some(w) = workers {
            exec.set_pool(Some(Arc::new(tutti_core::WorkerPool::builder(w).build())));
        }
        let mut clock = tutti_types::FrameClock::new(
            tutti_types::Beat(0.0),
            tutti_types::Bpm(120.0),
            SampleRate(48_000.0),
        );
        let mut bufs = vec![vec![0.0f32; 128]; 200];
        let mut all = vec![Vec::new(); 200];
        for (b, n) in lane_blocks(12_000).into_iter().enumerate() {
            lane_moves(&mut ed, &controls, b);
            let t = lanes_transport(&clock);
            {
                let mut refs: Vec<&mut [f32]> = bufs.iter_mut().map(|b| &mut b[..]).collect();
                exec.process(n, &t, &[], &mut refs);
            }
            clock.advance(tutti_types::Samples(n), lanes_loop());
            ed.collect();
            for (a, b) in all.iter_mut().zip(&bufs) {
                a.extend(b[..n].iter().map(|v| v.to_bits()));
            }
        }
        assert_eq!(exec.is_parallel(), workers.is_some(), "{workers:?}");
        all
    };
    let serial = render(None);
    assert!(
        serial.iter().all(|c| c.iter().any(|&v| v != c[0])),
        "every lane moves its port"
    );
    for w in [2, 3, 4, 8] {
        let par = render(Some(w));
        for (k, (a, b)) in serial.iter().zip(&par).enumerate() {
            assert!(a == b, "{w} participants, lane {k}");
        }
    }
}

/// 200 control lanes never allocate on the audio thread, serial and on a
/// pool whose helpers run under the gate too, with a republish of every
/// program, knob writes and range edits between gated blocks (on the
/// control thread, outside the gate).
///
/// Mutation (run): a `Vec::with_capacity(8)` per block in
/// `ControlLaneNode::process` → aborts.
#[test]
fn two_hundred_lanes_are_allocation_free_serial_and_on_a_pool() {
    use std::sync::Arc;
    for workers in [None, Some(4)] {
        let (mut ed, mut exec, controls) = lanes_200(128);
        if let Some(w) = workers {
            let pool = tutti_core::WorkerPool::builder(w)
                .spin(std::time::Duration::from_micros(1))
                .idle(std::time::Duration::from_micros(1))
                .build();
            exec.set_pool(Some(Arc::new(GatedPool(pool))));
        }
        let mut clock = tutti_types::FrameClock::new(
            tutti_types::Beat(0.0),
            tutti_types::Bpm(120.0),
            SampleRate(48_000.0),
        );
        let mut bufs = vec![vec![0.0f32; 128]; 200];
        let mut refs: Vec<&mut [f32]> = bufs.iter_mut().map(|b| &mut b[..]).collect();
        let blocks = lane_blocks(24_000);
        for (b, &n) in blocks.iter().enumerate() {
            lane_moves(&mut ed, &controls, b);
            let t = lanes_transport(&clock);
            if b < 40 {
                exec.process(n, &t, &[], &mut refs);
            } else {
                assert_no_alloc::assert_no_alloc(|| exec.process(n, &t, &[], &mut refs));
            }
            clock.advance(tutti_types::Samples(n), lanes_loop());
            ed.collect();
        }
        assert_eq!(exec.is_parallel(), workers.is_some(), "{workers:?}");
        drop(refs);
        assert!(
            bufs.iter().all(|b| b.iter().any(|&v| v != 0.0)),
            "every lane drove its port: the gate walked the lanes"
        );
    }
}
