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
//! The address checks write through `ParamSet::set`, and the end-to-end check
//! drives a control lane whose knob is the filter's cell.

use std::sync::atomic::Ordering;
use tutti_fixtures::signal::peak;
use tutti_types::MaxBlock;

use tutti_curve::Law;
use tutti_graph::NodeKey;
use tutti_graph::{
    Cx, Editor, EventOut, GraphBuilder, Io, Node, ParamIn, ParamNode, ParamRange, ParamSet,
    Prepare, Renderer, Shape, Status, Unforkable,
};
use tutti_graph::{OutPort, Source};
use tutti_modulation::{
    Amount, Clock, ControlProgram, Layer, LayerId, LfoShape, Polarity, Waveform,
};
use tutti_nodes::automation::ControlLaneNode;
use tutti_nodes::{
    CompressorControls, CompressorNode, LadderFilterNode, LadderType, LfoNode, LimiterControls,
    LimiterNode, ModDelayNode, PhaserControls, PhaserNode, Stages, SvfFilterNode, SvfType,
};
use tutti_types::{BeatDuration, Db, Depth, Hz, PhaseIncrement, Q, Resonance, SampleRate, Samples};
use tutti_types::{BusTopology, ChannelLayout, UnitParam};

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
    SvfFilterNode::<f32>::with_channels(
        ChannelLayout::STEREO,
        SvfType::LowPass,
        Hz::new(1000.0),
        Q::new(0.7),
    )
}

#[test]
fn filter_cutoff_write_moves_the_nodes_own_cell() {
    let node = svf();
    // The cell a lane takes as its knob is the one the node reads per block.
    let cell = node.param_set().cell(UnitParam::Cutoff).expect("addressed");
    assert!(set(&node, UnitParam::Cutoff, 1500.0), "cutoff is addressed");
    assert_eq!(cell.load(Ordering::Acquire), 1500.0);
    assert_eq!(node.param_set().get(UnitParam::Cutoff), Some(1500.0));
}

#[test]
fn a_param_the_set_lacks_has_no_address() {
    // The SVF has no Drive param.
    assert!(!addresses(&svf(), UnitParam::Drive));
    assert!(!set(&svf(), UnitParam::Drive, 1.0));
}

#[test]
fn ladder_addresses_q_and_drive_but_not_feedback() {
    let node = LadderFilterNode::<f32>::with_channels(
        ChannelLayout::STEREO,
        LadderType::LP24,
        Hz::new(800.0),
        Resonance::new(0.5),
    );
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
        Shape::source_unassigned(ChannelLayout::MONO).with_params(&[UnitParam::Cutoff])
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
#[test]
fn an_lfo_lane_sweeps_around_a_real_filters_cutoff_cell() {
    let filter = svf();
    let (lo, hi) = (500.0, 1500.0);
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate(48_000.0),
        MaxBlock::of(Samples(64)),
    ));
    let probe = NodeKey(1);
    ed.insert(probe, "probe", Unforkable(Probe));
    ed.spec_mut()
        .topology
        .set_outputs(vec![Source::Node(OutPort {
            node: probe,
            port: 0,
        })]);
    let at = ParamIn {
        node: probe,
        param: UnitParam::Cutoff.into(),
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
        wave: Waveform::Shape {
            shape: LfoShape::Sine,
            seed: 0,
        },
        amount: Amount {
            depth: Depth::of(0.25),
            polarity: Polarity::Bipolar,
            curve: Law::Linear,
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

/// A chorus, `input` on both channels;
/// when `lane` is `Some(depth)`, its rate is driven by a control lane
/// whose knob is the chorus's own rate cell and whose one layer is a
/// constant `depth` of the rate port's 0..10 Hz span.
fn render_chorus(
    node: ModDelayNode,
    lane: Option<f32>,
    input: &[f32],
) -> (Vec<Vec<f32>>, ParamSet) {
    let mut g = GraphBuilder::new(ChannelLayout::STEREO, ChannelLayout::STEREO);
    let (n, params) = g.add_with_controls(node);
    g.connect_input(0, n, 0)
        .connect_input(1, n, 1)
        .connect_output(n, 0, 0)
        .connect_output(n, 1, 1);
    let at = ParamIn {
        node: n,
        param: UnitParam::Rate.into(),
    };
    g.spec_mut().set_param_range(at, ParamRange::new(0.0, 10.0));
    let mut r = g
        .renderer(Prepare::new(
            SampleRate(48_000.0),
            MaxBlock::of(Samples(128)),
        ))
        .expect("builds");
    if let Some(depth) = lane {
        let ed = r.editor_mut();
        let layer = Layer {
            id: LayerId(1),
            clock: Clock::Beats {
                per_cycle: BeatDuration(1.0),
                phase: PhaseIncrement(0.0),
            },
            wave: Waveform::Const(1.0),
            amount: Amount {
                depth: Depth::of(depth),
                polarity: Polarity::Bipolar,
                curve: Law::Linear,
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
    (r.render_input(&[input, input]), params)
}

/// The ramp `x[n] = n`: through a linearly interpolated delay of `d` frames it comes out
/// as `n - d`, exactly (a line read between two of its points is on the line), so a
/// wet-only swept delay with no feedback shows its delay at every frame.
fn ramp(frames: usize) -> Vec<f32> {
    (0..frames).map(|n| n as f32).collect()
}

/// The delay, in frames, that a wet-only, feedback-free swept delay applied at frame `n`
/// of its render of [`ramp`].
fn delay_at(out: &[f32], n: usize) -> f32 {
    n as f32 - out[n]
}

/// A chorus on the bus `input` with no feedback, so its output is the input delayed by
/// the sweep alone.
fn dry_chorus(input: BusTopology) -> ModDelayNode {
    let n = ModDelayNode::chorus(input);
    assert!(set(&n, UnitParam::Feedback, 0.0));
    n
}

/// **A control lane modulates a chorus's rate**, around the chorus's own rate cell: a
/// +2 Hz offset on the 1 Hz knob sweeps the delay at 3 Hz, and the cell still holds the
/// knob (the lane sends to the port and never writes it, doc 014 §4.2).
///
/// The definition of the sweep: channel `c`'s delay is `base + depth * sin(2π (f t +
/// φ_c))`, with the chorus's 10 ms base, 5 ms depth and a quarter cycle between the
/// stereo channels. It is solved exactly at the stream's control points, every 16 frames
/// from the start, which is where it is read here, through the ramp (the delay is the
/// ramp's lag). At 3 Hz the two channels at frame 4800 sit at `sin(0.6π)` and `sin(1.1π)`
/// of the depth; at 1 Hz they would sit at `sin(0.2π)` and `sin(0.7π)`, 87 and 128
/// frames away.
#[test]
fn a_lane_modulates_a_chorus_rate_around_its_own_cell() {
    const FRAMES: usize = 4_800;
    const RATE: f64 = 3.0;
    let sr = 48_000.0_f64;
    let (out, params) = render_chorus(dry_chorus(BusTopology::STEREO), Some(0.2), &ramp(FRAMES));
    // Past the longest delay (15 ms, 720 frames) the line holds the ramp alone.
    for (c, channel) in out.iter().enumerate() {
        for n in (768..FRAMES).step_by(16) {
            let phase = RATE * n as f64 / sr + 0.25 * c as f64;
            let want = (0.010 + 0.005 * (std::f64::consts::TAU * phase).sin()) * sr;
            let got = delay_at(channel, n);
            assert!(
                (f64::from(got) - want).abs() < 0.1,
                "channel {c} frame {n}: a delay of {got} frames, the 3 Hz sweep has {want}"
            );
        }
    }
    assert_eq!(
        params.get(UnitParam::Rate),
        Some(1.0),
        "the knob is untouched"
    );
}

#[test]
fn chorus_rate_write_moves_the_nodes_own_cell() {
    let node = ModDelayNode::chorus(BusTopology::STEREO);
    let cell = node.param_set().cell(UnitParam::Rate).expect("addressed");
    let base = cell.load(Ordering::Acquire);
    assert!(
        set(&node, UnitParam::Rate, base + 2.0),
        "chorus rate is addressed"
    );
    assert_eq!(cell.load(Ordering::Acquire), base + 2.0);

    // A param it doesn't expose has no address.
    assert!(!addresses(&node, UnitParam::Cutoff));
}

/// The compressor's makeup has one address, `GainDb`; `Makeup` is not in
/// its set.
#[test]
fn compressor_makeup_is_addressed_as_gain_db() {
    let node = CompressorNode::stereo(CompressorControls {
        threshold: tutti_types::Db(-12.0),
        ratio: tutti_types::CompressionRatio(4.0),
        attack: tutti_types::Seconds(0.01),
        release: tutti_types::Seconds(0.1),
        ..CompressorControls::DEFAULT
    });
    let makeup = node
        .param_set()
        .cell(UnitParam::GainDb)
        .expect("makeup is GainDb");
    assert!(set(&node, UnitParam::GainDb, 6.0), "makeup is GainDb");
    assert_eq!(makeup.load(Ordering::Acquire), 6.0);
    assert!(!addresses(&node, UnitParam::Makeup));
}

#[cfg(feature = "convolution")]
#[test]
fn convolver_addresses_nothing() {
    // Room size is baked into the IR, the gain has no address and the mix is
    // the graph's (`NodeMix::ADDR`, not a node's param).
    // A minimal unit IR is fine — only the param surface is probed.
    let node = tutti_nodes::ConvolverNode::shared_ir(2u16, &[1.0], 64);
    assert!(!addresses(&node, UnitParam::Wet));
    assert!(!addresses(&node, UnitParam::RoomSize));
    assert!(!addresses(&node, UnitParam::Cutoff));
}

/// A second and a quarter (60 000 frames) of a free-running 1 Hz sine
/// `LfoNode` at depth 0.25, with a control lane on `param` (knob: the LFO's
/// own cell) adding a constant `depth` of `range`, and the cell's value
/// after the render.
fn render_lfo(param: UnitParam, range: ParamRange, depth: f32) -> (Vec<f32>, f32) {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let lfo = LfoNode::new(LfoShape::Sine)
        .with_frequency(Hz(1.0))
        .with_depth(Depth::of(0.25));
    let (n, params) = g.add_with_controls(lfo);
    g.connect_output(n, 0, 0);
    let at = ParamIn {
        node: n,
        param: param.into(),
    };
    g.spec_mut().set_param_range(at, range);
    let mut r = g
        .renderer(Prepare::new(
            SampleRate(48_000.0),
            MaxBlock::of(Samples(128)),
        ))
        .expect("builds");
    let knob = params.cell(param).expect("the LFO addresses it");
    let ed = r.editor_mut();
    let layer = Layer {
        id: LayerId(1),
        clock: Clock::Beats {
            per_cycle: BeatDuration(1.0),
            phase: PhaseIncrement(0.0),
        },
        wave: Waveform::Const(1.0),
        amount: Amount {
            depth: Depth::of(depth),
            polarity: Polarity::Bipolar,
            curve: Law::Linear,
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
    let out = r.render(60_000).swap_remove(0);
    (out, knob.load(Ordering::Acquire))
}

/// **A lane drives an `LfoNode`'s depth and rate**, each
/// around the LFO's own cell. The depth knob at 0.25 plus half of a 0..1
/// range swings the 1 Hz sine to ±0.75; the 1 Hz rate knob plus a fifth of a
/// 0..10 Hz range runs it at 3 Hz. Over a second and a quarter, 3 Hz rises
/// through zero at frames 16 000, 32 000 and 48 000, and 1 Hz (the depth
/// render) once, at frame 48 000. The LFO's clock is exact, so each crossing
/// is pinned to within a frame of where it belongs; a count alone would
/// miss a clock that drifts. Neither cell moves.
#[test]
fn a_lane_drives_an_lfo_nodes_depth_and_rate() {
    let (out, knob) = render_lfo(UnitParam::Depth, ParamRange::new(0.0, 1.0), 0.5);
    let peak = out.iter().copied().fold(0.0f32, |m, v| m.max(v.abs()));
    assert!((peak - 0.75).abs() < 1e-3, "peaks at 0.75, not {peak}");
    assert_eq!(knob, 0.25, "the lane never writes the depth cell");
    assert_crossings(&out, 48_000, 1);

    let (out, knob) = render_lfo(UnitParam::Rate, ParamRange::new(0.0, 10.0), 0.2);
    assert_crossings(&out, 16_000, 3);
    assert_eq!(knob, 1.0, "the lane never writes the rate cell");
}

/// `out` rises through zero exactly `count` times, the `k`th (from 1)
/// within one frame of `k * period`. A crossing's frame is the first one at
/// or above zero.
fn assert_crossings(out: &[f32], period: usize, count: usize) {
    let rising: Vec<usize> = out
        .windows(2)
        .enumerate()
        .filter(|(_, w)| w[0] < 0.0 && w[1] >= 0.0)
        .map(|(i, _)| i + 1)
        .collect();
    assert_eq!(rising.len(), count, "rising crossings at {rising:?}");
    for (k, &frame) in rising.iter().enumerate() {
        let want = (k + 1) * period;
        assert!(
            frame.abs_diff(want) <= 1,
            "crossing {} at frame {frame}, not {want} ({rising:?})",
            k + 1
        );
    }
}

/// `input` (mono) through `node` at 48 kHz in 128-frame blocks, with a
/// control lane on `param` whose knob is the node's own cell, the port's
/// range `range`, and whose one layer adds a constant `depth` of that range
/// (negative: below the knob). The lane is wired before the first block.
fn render_driven<N>(
    node: N,
    param: UnitParam,
    range: ParamRange,
    depth: f32,
    input: &[f32],
) -> Vec<f32>
where
    N: tutti_graph::IntoNode<Controls = ParamSet>,
{
    let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::MONO);
    let (n, params) = g.add_with_controls(node);
    g.connect_input(0, n, 0).connect_output(n, 0, 0);
    let at = ParamIn {
        node: n,
        param: param.into(),
    };
    g.spec_mut().set_param_range(at, range);
    let mut r = g
        .renderer(Prepare::new(
            SampleRate(48_000.0),
            MaxBlock::of(Samples(128)),
        ))
        .expect("builds");
    let ed = r.editor_mut();
    let layer = Layer {
        id: LayerId(1),
        clock: Clock::Beats {
            per_cycle: BeatDuration(1.0),
            phase: PhaseIncrement(0.0),
        },
        wave: Waveform::Const(1.0),
        amount: Amount {
            depth: Depth::of(depth),
            polarity: Polarity::Bipolar,
            curve: Law::Linear,
        },
        depth_by: None,
    };
    let program = ControlProgram::new(None, vec![layer], vec![]).expect("valid");
    let knob = params.cell(param).expect("the node addresses it");
    let range = ed
        .param_range_cell(at)
        .expect("the node declares it as a port");
    let lane = ControlLaneNode::for_unit(param, program, Some(knob), Some(range));
    ed.insert(NodeKey(9_000), "lane", lane);
    ed.spec_mut().connect_param(
        at,
        EventOut {
            node: NodeKey(9_000),
            port: 0,
        },
    );
    ed.commit().expect("the lane commits");
    r.render_input(&[input]).swap_remove(0)
}

/// **A limiter driven to a release below its floor still releases.** A lane
/// on the release port, ranged `-1..=1` s, drives it to −0.4 s, below the
/// 1 ms `set_release` floors at. A full-scale burst into a −12 dB limiter,
/// then a quiet tone under the threshold: the output is finite, and the
/// quiet tone comes back at its own level (the gain reduction released),
/// not held down at the burst's, and it comes back at the 1 ms floor's
/// pace, not instantly.
///
/// Two things keep it releasing: `clamp_release` floors the driven value at
/// 1 ms, and `Seconds::one_pole_coeff` makes a time that is not above zero
/// instant rather than a freeze.
#[test]
fn a_limiter_driven_below_its_release_floor_still_releases() {
    const SR: f32 = 48_000.0;
    let tone = |amp: f32, frames: usize| -> Vec<f32> {
        (0..frames)
            .map(|i| amp * (std::f32::consts::TAU * 440.0 * i as f32 / SR).sin())
            .collect()
    };
    // The burst's length, and the limiter's latency: its 5 ms lookahead is
    // 240 frames at 48 kHz, and the ring delays by one less.
    const BURST: usize = 12_000;
    const LATENCY: usize = 239;
    let mut input = tone(1.0, BURST);
    input.extend(tone(0.05, 36_000));
    let out = render_driven(
        LimiterNode::mono(LimiterControls {
            threshold: Db::new(-12.0),
            ceiling: Db::new(-12.0),
            ..LimiterControls::DEFAULT
        }),
        UnitParam::Release,
        ParamRange::new(-1.0, 1.0),
        -0.25,
        &input,
    );
    assert!(out.iter().all(|v| v.is_finite()), "finite throughout");
    // The first millisecond of the quiet tone out of the lookahead: at the
    // 1 ms floor the gain is still recovering from the burst's 0.25
    // (about 0.63 of the input over the window); an instant release
    // would pass it whole (about 0.99).
    let quiet = BURST + LATENCY;
    let ms = 48;
    let gain = |x: &[f32]| x.iter().map(|v| v.abs()).sum::<f32>();
    let early = gain(&out[quiet + 1..quiet + 1 + ms]) / gain(&input[BURST + 1..BURST + 1 + ms]);
    assert!(
        early < 0.8,
        "a driven negative release is the 1 ms floor, not instant: {early}"
    );
    let tail = peak(&out[out.len() - 12_000..]);
    assert!(
        (tail - 0.05).abs() < 0.005,
        "the quiet tone comes back at 0.05, not held down: {tail}"
    );
}

/// **A phaser driven past its depth range stays stable.** A lane on the
/// depth port, ranged `-8..=8`, drives it to 8 and, separately, to −7.5. Past
/// 1 the sweep would cross Nyquist and below 0 it would go below 0 Hz;
/// either puts the all-pass coefficient outside `-1..=1`, where the stages
/// diverge. Clamped (`Depth::clamped` on the depth, the sweep held
/// inside `1 Hz..0.49 sr`) the output stays bounded by what the input and a
/// 0.5 feedback can make.
///
/// That a depth past 1 sweeps the range and no further is stated from the definition in
/// `tutti-nodes`' `a_depth_lane_past_one_sweeps_the_whole_range_and_no_further` (it was a
/// render against another render, deleted).
#[test]
fn a_phaser_driven_past_its_depth_range_stays_stable() {
    let mut state = 0x9e37_79b9_u32;
    let noise: Vec<f32> = (0..48_000)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            0.5 * ((state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0)
        })
        .collect();
    for depth in [0.5, -0.5] {
        // Knob 0.5 plus `depth` of the 16-wide range: 8.5 (held at the
        // range's 8) and -7.5.
        let out = render_driven(
            PhaserNode::mono(Stages::of(4), PhaserControls::DEFAULT),
            UnitParam::Depth,
            ParamRange::new(-8.0, 8.0),
            depth,
            &noise,
        );
        let p = peak(&out);
        assert!(
            p.is_finite() && p < 4.0,
            "driven at {depth} of the range: bounded, peak {p}"
        );
    }
}

/// **A chorus's depth lane past its range is held at the sweep's bound.** Driven to 0.5 s
/// of depth, past the chorus's 40 ms `depth_max`, the delay sweeps `base ± 40 ms` exactly:
/// at each control point it is `10 ms + 40 ms * sin(2π f t)`, floored at one frame, read
/// through the ramp. It peaks at 50 ms (2400 frames, the whole line) only at the sine's
/// crest; a depth held anywhere past 40 ms would sit at the line's end for a span of the
/// cycle instead.
#[test]
fn a_chorus_depth_lane_past_its_range_is_held_at_the_sweeps_bound() {
    const FRAMES: usize = 9_600;
    const RATE: f64 = 10.0;
    let sr = 48_000.0_f64;
    let n = dry_chorus(BusTopology::MONO);
    // 10 Hz, so the sweep turns twice in the render.
    assert!(set(&n, UnitParam::Rate, RATE as f32));
    // Knob 5 ms plus 0.495 of a 1 s range: 0.5 s.
    let out = render_driven(
        n,
        UnitParam::Depth,
        ParamRange::new(0.0, 1.0),
        0.495,
        &ramp(FRAMES),
    );
    // Past the line's length the line holds the ramp alone. The tolerance is a frame: the
    // sweep runs 2.5 frames of delay per frame at its steepest, and the f32 phase drifts
    // by some 1e-5 of a cycle over the render; a depth past 40 ms misses by hundreds.
    let mut most = f32::MIN;
    for n in (2_400..FRAMES).step_by(16) {
        let phase = RATE * n as f64 / sr;
        let want = ((0.010 + 0.040 * (std::f64::consts::TAU * phase).sin()) * sr).max(1.0);
        let got = delay_at(&out, n);
        most = most.max(got);
        assert!(
            (f64::from(got) - want).abs() < 1.0,
            "frame {n}: a delay of {got} frames, a 40 ms sweep has {want}"
        );
    }
    assert!(most <= 2_400.5, "never past the line: {most}");
}

/// **A chorus's rate lane below its range is held at the 0.01 Hz floor.** Driven to −5 Hz,
/// the sweep runs forwards at 0.01 Hz: at frame `n` the delay is `10 ms + 5 ms *
/// sin(2π 0.01 n / 48000)`, which half a second in is 7.5 frames past the base; a rate of
/// 0 would hold it at the base, and −5 Hz would have swept it through whole cycles.
#[test]
fn a_chorus_rate_lane_below_its_range_is_held_at_the_floor() {
    const FRAMES: usize = 24_000;
    let sr = 48_000.0_f64;
    // Knob 1 Hz minus 0.3 of a 20 Hz range: −5 Hz.
    let out = render_driven(
        dry_chorus(BusTopology::MONO),
        UnitParam::Rate,
        ParamRange::new(-10.0, 10.0),
        -0.3,
        &ramp(FRAMES),
    );
    for n in (768..FRAMES).step_by(16) {
        let phase = 0.01 * n as f64 / sr;
        let want = (0.010 + 0.005 * (std::f64::consts::TAU * phase).sin()) * sr;
        let got = delay_at(&out, n);
        assert!(
            (f64::from(got) - want).abs() < 0.05,
            "frame {n}: a delay of {got} frames, the 0.01 Hz sweep has {want}"
        );
    }
}
