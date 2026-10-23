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
//! Ported from routes through a `ModMatrix` onto an
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
    CompressorNode, LadderFilterNode, LadderType, LfoNode, LimiterNode, ModDelayNode, PhaserNode,
    SvfFilterNode, SvfType,
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
/// §4.2). The rate is a port ([`MOD_DELAY_PARAMS`]). The claim the old
/// `chorus_rate_route_moves_the_nodes_own_cell` made, on the lane.
///
/// Mutation (run): `ModDelayNode` not declaring its ports (the
/// `.with_params(&MOD_DELAY_PARAMS)` dropped) → the lane's port does not
/// exist and the connect is refused at commit → fails. Mutation (run):
/// `LfoDrive::fill_block` ignoring the driven rate → the chorus runs at
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

/// **A lane drives an `LfoNode`'s depth and rate** ([`LFO_PARAMS`]), each
/// around the LFO's own cell. The depth knob at 0.25
/// plus half of a 0..1 range swings the 1 Hz sine to ±0.75; the 1 Hz rate
/// knob plus a fifth of a 0..10 Hz range runs it at 3 Hz, three rising
/// zero crossings in a second where 1 Hz has one. Neither cell moves.
///
/// Mutation (run): the LFO reading its depth cell, not the driven
/// port (`block_driven(&io, 0)` replaced by `None`) → it peaks at 0.25 →
/// fails. Mutation (run): reading its rate cell (`block_driven(&io,
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
    let at = ParamIn { node: n, param };
    g.spec_mut().set_param_range(at, range);
    let mut r = g
        .renderer(Prepare::new(SampleRate(48_000.0), Samples(128)))
        .expect("builds");
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

/// Peak magnitude of `x`.
fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0f32, |m, v| m.max(v.abs()))
}

/// **A limiter driven to a release below its floor still releases.** A lane
/// on the release port, ranged `-1..=1` s, drives it to −0.4 s, below the
/// 1 ms `set_release` floors at. A full-scale burst into a −12 dB limiter,
/// then a quiet tone under the threshold: the output is finite, and the
/// quiet tone comes back at its own level (the gain reduction released),
/// not held down at the burst's.
///
/// Two things keep it releasing: `clamp_release` floors the driven value at
/// 1 ms, and `Seconds::one_pole_coeff` makes a time that is not above zero
/// instant rather than a freeze. Mutations (run): `update_coefficients`
/// without `clamp_release` alone → the release is instant and the tone
/// still comes back → passes; that and `one_pole_coeff` returning 0 (a
/// hold) for a non-positive time → the envelope freezes at the burst's
/// reduction and the tone stays silent → fails.
#[test]
fn a_limiter_driven_below_its_release_floor_still_releases() {
    const SR: f32 = 48_000.0;
    let tone = |amp: f32, frames: usize| -> Vec<f32> {
        (0..frames)
            .map(|i| amp * (std::f32::consts::TAU * 440.0 * i as f32 / SR).sin())
            .collect()
    };
    let mut input = tone(1.0, 12_000);
    input.extend(tone(0.05, 36_000));
    let out = render_driven(
        LimiterNode::with_channels(ChannelLayout::MONO, -12.0, -12.0),
        UnitParam::Release,
        ParamRange::new(-1.0, 1.0),
        -0.25,
        &input,
    );
    assert!(out.iter().all(|v| v.is_finite()), "finite throughout");
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
/// diverge. Clamped (`Depth::new_clamped` on the depth, the sweep held
/// inside `1 Hz..0.49 sr`) the output stays bounded by what the input and a
/// 0.5 feedback can make.
///
/// And the depth gets `set_depth`'s clamp: driven to 8 it sounds as it
/// does driven to exactly 1 (the knob at 1, nothing added).
///
/// Mutation (run): `LinearModMix::load` without
/// `Depth::new_clamped` → the sweep hold keeps the +8 drive bounded, but it
/// sweeps unlike depth 1 → the last assertion fails. Mutation (run):
/// `allpass_coeff` without the sweep hold → the −7.5 drive
/// diverges → fails. (A depth of −1 reaches the second through `set_depth` itself, so
/// the hold also fixes a divergence that predates the port.)
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
            PhaserNode::with_channels(ChannelLayout::MONO, 4),
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
    let phaser_at = |knob: f32, depth: f32| {
        let n = PhaserNode::with_channels(ChannelLayout::MONO, 4);
        n.set_depth(knob);
        render_driven(
            n,
            UnitParam::Depth,
            ParamRange::new(-8.0, 8.0),
            depth,
            &noise,
        )
    };
    let (past, full) = (phaser_at(1.0, 0.4375), phaser_at(1.0, 0.0));
    for (i, (a, b)) in past.iter().zip(&full).enumerate() {
        assert!(
            (a - b).abs() < 1e-5,
            "frame {i}: driven to 8 {a} vs depth 1 {b}"
        );
    }
}

/// **A chorus's driven depth and rate get their setters' clamps.** Driven
/// to 0.5 s of depth (range `0..=1`), past the chorus's 40 ms
/// `depth_max`, it sounds as it does at a 40 ms knob; driven to −5 Hz
/// (range `-10..=10`), below `set_rate`'s 0.01 Hz floor, as it does at a
/// 0.01 Hz knob.
///
/// Mutation (run): the block's depth without
/// `ModDelayConfig::clamp_depth` → a 0.5 s sweep reads past the line's
/// capacity, unlike 40 ms → fails. Mutation (run):
/// `LfoDrive::fill_block` taking a driven rate unclamped → the LFO runs
/// backwards at 5 Hz → fails.
#[test]
fn a_chorus_driven_past_its_depth_and_rate_gets_the_setters_clamps() {
    let mut state = 0x2545_f491_u32;
    let noise: Vec<f32> = (0..24_000)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            0.5 * ((state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0)
        })
        .collect();
    let chorus = |depth: f32, rate: f32| {
        let n = ModDelayNode::chorus(ChannelLayout::MONO);
        n.set_depth(depth);
        n.set_rate(rate);
        n
    };
    let close = |a: &[f32], b: &[f32], what: &str| {
        for (i, (x, y)) in a.iter().zip(b).enumerate() {
            assert!((x - y).abs() < 1e-5, "{what}: frame {i}: {x} vs {y}");
        }
    };
    // Knob 5 ms plus 0.495 of a 1 s range: 0.5 s.
    let past = render_driven(
        chorus(0.005, 1.0),
        UnitParam::Depth,
        ParamRange::new(0.0, 1.0),
        0.495,
        &noise,
    );
    let at_max = render_driven(
        chorus(0.04, 1.0),
        UnitParam::Depth,
        ParamRange::new(0.0, 1.0),
        0.0,
        &noise,
    );
    close(&past, &at_max, "depth past 40 ms");
    // Knob 1 Hz minus 0.3 of a 20 Hz range: -5 Hz.
    let below = render_driven(
        chorus(0.005, 1.0),
        UnitParam::Rate,
        ParamRange::new(-10.0, 10.0),
        -0.3,
        &noise,
    );
    let at_floor = render_driven(
        chorus(0.005, 0.01),
        UnitParam::Rate,
        ParamRange::new(-10.0, 10.0),
        0.0,
        &noise,
    );
    close(&below, &at_floor, "rate below 0.01 Hz");
}
