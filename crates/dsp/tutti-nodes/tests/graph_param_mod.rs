//! This crate's modulatable nodes under the graph's compiler-owned param
//! modulation (design doc 013 item 6), run as they run in an engine: each
//! through `Legacy`, its params fed per 64-frame chunk through its
//! `ParamFeed`.
//!
//! These replace the tests of the per-param sub-graph the graph made
//! obsolete (`AtomicSourceNode → ParamSumNode ← ParamShaperNode`, wired into
//! extra input channels a node had to be born with): `born_with_ports.rs`,
//! `param_writer_ownership.rs`, `audio_rate_param_mod.rs` and
//! `idle_chain_cost.rs`. What each pinned, and where it went:
//!
//! - "an unfed port reads the param as 0", "the base chain is mandatory",
//!   "the idle chain costs two nodes per param": gone by construction — an
//!   unconnected param reads its base (`tutti-graph`'s
//!   `an_unconnected_param_reads_its_base_never_zero`), and an unmodulated
//!   param adds nothing to the plan
//!   (`an_unmodulated_param_adds_nothing_to_the_plan`, below);
//! - "the port a node advertises is the one its DSP reads, at every width"
//!   (`param_ports.rs`): `each_fed_param_is_the_one_the_dsp_reads`, below;
//! - "the edge changes what the node produces", "the sum folds base + N
//!   offsets and clamps", "a crossed range is survivable", "the shaper
//!   agrees with the control-rate shaping": below, and
//!   `param_mod_oracle.rs` (bit for bit against the old nodes);
//! - "the authored value must land on the sum's base cell", "control rate
//!   and audio rate share one base cell": the base is now the node's own
//!   control, so an authored write — or a control-rate target mirroring
//!   into that cell — moves the modulated param
//!   (`an_authored_write_moves_the_base_under_modulation`);
//! - "a param port is clobbered by `pipe_input`", "N edges land on ports
//!   1..N": no ports, so nothing to clobber or number.

use std::sync::atomic::Ordering;

use tutti_core::AudioUnit;
use tutti_graph::{
    GraphBuilder, ParamFrom, ParamIn, ParamRange, ParamShaping, Prepare, Renderer, PARAM_DECLICK,
};
use tutti_nodes::testing::Const;
use tutti_nodes::{
    BrickwallLimiterNode, BusStripNode, CompressorNode, DelayLineNode, DistortionNode, GateNode,
    LadderFilterNode, LadderType, LimiterNode, ShapeKind, SvfFilterNode, SvfType,
};
use tutti_types::graph::OutPort;
use tutti_types::{Amplitude, ChannelLayout, SampleRate, Samples, UnitParam};

const FRAMES: usize = 24_000;
/// Where [`each_fed_param_is_the_one_the_dsp_reads`] starts comparing: far
/// past the connection's declick, and past the slowest node's memory of it
/// (a compressor's 50 ms release, a filter's integrators).
const SETTLED: usize = 20_000;

fn noise(seed: u32) -> Vec<f32> {
    let mut state = seed.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    (0..FRAMES)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            0.8 * ((state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0)
        })
        .collect()
}

/// `node` with every input on a global input carrying noise, its outputs
/// the graph's, and — when `fed` — `param` driven to exactly `v` by a
/// constant source through a degenerate range (`v..=v`), whatever the base.
/// Renders `FRAMES` in 100-frame blocks (so `Legacy` chunks 64 + 36).
fn render(node: Box<dyn AudioUnit>, fed: Option<(UnitParam, f32)>) -> Vec<Vec<f32>> {
    let (ins, outs) = (node.inputs(), node.outputs());
    let mut g = GraphBuilder::new(ChannelLayout::from(ins), ChannelLayout::from(outs));
    let n = g.add_unit(node);
    for c in 0..ins {
        g.connect_input(c, n, c);
    }
    for c in 0..outs {
        g.connect_output(n, c, c);
    }
    if let Some((param, v)) = fed {
        let src = g.add_unit(Box::new(Const::mono(0.0)));
        let at = ParamIn { node: n, param };
        g.spec_mut().connect_param(
            at,
            ParamFrom::Audio(OutPort { node: src, port: 0 }),
            ParamShaping::Identity,
        );
        g.spec_mut().set_param_range(at, ParamRange::new(v, v));
    }
    let mut r = g
        .renderer(Prepare::new(SampleRate(48_000.0), Samples(100)))
        .expect("builds");
    let input: Vec<Vec<f32>> = (0..ins).map(|c| noise(c as u32 + 7)).collect();
    let refs: Vec<&[f32]> = input.iter().map(Vec::as_slice).collect();
    r.render_input(&refs)
}

/// One modulatable param of one node type: how to build the node at a
/// width with the param's control at `v` (or its default), and two values
/// far enough apart to be heard.
struct Case {
    name: &'static str,
    param: UnitParam,
    make: fn(usize, Option<f32>) -> Box<dyn AudioUnit>,
    lo: f32,
    hi: f32,
}

fn cases() -> Vec<Case> {
    fn svf(w: usize) -> SvfFilterNode<f64> {
        SvfFilterNode::with_channels(w, SvfType::LowPass, 1_000.0, 0.707)
    }
    fn ladder(w: usize) -> LadderFilterNode<f64> {
        LadderFilterNode::with_channels(w, LadderType::LP24, 1_000.0, 0.3)
    }
    fn delay(w: usize) -> DelayLineNode {
        let d = DelayLineNode::with_channels(w, 0.01, 0.0003, 0.5);
        d.set_mix(0.5);
        d
    }
    fn comp(w: usize) -> CompressorNode {
        CompressorNode::with_channels(-20.0, 8.0, 0.001, 0.05, w as u8)
    }
    fn gate(w: usize) -> GateNode {
        GateNode::with_channels(-30.0, 0.001, 0.004, 0.02, w as u8)
    }
    fn lim(w: usize) -> LimiterNode {
        LimiterNode::with_channels(w, -6.0, -0.3)
    }
    vec![
        Case {
            name: "svf cutoff",
            param: UnitParam::Cutoff,
            make: |w, v| {
                let n = svf(w);
                if let Some(v) = v {
                    n.set_frequency(v);
                }
                Box::new(n)
            },
            lo: 200.0,
            hi: 8_000.0,
        },
        Case {
            name: "svf q",
            param: UnitParam::Q,
            make: |w, v| {
                let n = svf(w);
                if let Some(v) = v {
                    n.set_q(v);
                }
                Box::new(n)
            },
            lo: 0.5,
            hi: 8.0,
        },
        Case {
            name: "ladder cutoff",
            param: UnitParam::Cutoff,
            make: |w, v| {
                let n = ladder(w);
                if let Some(v) = v {
                    n.set_frequency(v);
                }
                Box::new(n)
            },
            lo: 200.0,
            hi: 8_000.0,
        },
        Case {
            name: "ladder resonance",
            param: UnitParam::Q,
            make: |w, v| {
                let n = ladder(w);
                if let Some(v) = v {
                    n.set_resonance(v);
                }
                Box::new(n)
            },
            lo: 0.0,
            hi: 0.9,
        },
        Case {
            name: "ladder drive",
            param: UnitParam::Drive,
            make: |w, v| {
                let n = ladder(w);
                if let Some(v) = v {
                    n.set_drive(v);
                }
                Box::new(n)
            },
            lo: 1.0,
            hi: 8.0,
        },
        Case {
            name: "delay feedback",
            param: UnitParam::Feedback,
            make: |w, v| {
                let n = delay(w);
                if let Some(v) = v {
                    n.set_feedback(v);
                }
                Box::new(n)
            },
            lo: 0.0,
            hi: 0.9,
        },
        Case {
            name: "delay time",
            param: UnitParam::DelayTime,
            make: |w, v| {
                let n = delay(w);
                if let Some(v) = v {
                    n.set_delay_time(v);
                }
                Box::new(n)
            },
            lo: 0.0002,
            hi: 0.0008,
        },
        Case {
            name: "distortion drive",
            param: UnitParam::Drive,
            make: |w, v| {
                let n = DistortionNode::with_channels(w, ShapeKind::Tanh, 1.0);
                if let Some(v) = v {
                    n.set_drive(v);
                }
                Box::new(n)
            },
            lo: 0.5,
            hi: 5.0,
        },
        Case {
            name: "compressor threshold",
            param: UnitParam::Threshold,
            make: |w, v| {
                let n = comp(w);
                if let Some(v) = v {
                    n.set_threshold(v);
                }
                Box::new(n)
            },
            lo: -40.0,
            hi: 0.0,
        },
        Case {
            name: "gate threshold",
            param: UnitParam::Threshold,
            make: |w, v| {
                let n = gate(w);
                if let Some(v) = v {
                    n.set_threshold(v);
                }
                Box::new(n)
            },
            lo: -60.0,
            // Above the noise's peak: the gate stays shut.
            hi: 6.0,
        },
        Case {
            name: "limiter ceiling",
            param: UnitParam::Ceiling,
            make: |w, v| {
                let n = lim(w);
                if let Some(v) = v {
                    n.set_ceiling(v);
                }
                Box::new(n)
            },
            lo: -12.0,
            hi: -0.3,
        },
        Case {
            name: "limiter threshold",
            param: UnitParam::Threshold,
            make: |w, v| {
                let n = lim(w);
                if let Some(v) = v {
                    n.set_threshold(v);
                }
                Box::new(n)
            },
            lo: -20.0,
            hi: -1.0,
        },
        Case {
            name: "brickwall ceiling",
            param: UnitParam::Ceiling,
            make: |w, v| {
                let mut n = BrickwallLimiterNode::with_channels(w, 0.0);
                if let Some(v) = v {
                    n.set_ceiling(v);
                }
                Box::new(n)
            },
            lo: -12.0,
            hi: 0.0,
        },
        Case {
            name: "strip volume",
            param: UnitParam::Volume,
            make: |w, v| {
                let n = BusStripNode::with_channels(w);
                if let Some(v) = v {
                    n.set_volume(Amplitude(v));
                }
                Box::new(n)
            },
            lo: 0.2,
            hi: 0.9,
        },
        Case {
            name: "strip pan",
            param: UnitParam::Pan,
            make: |w, v| {
                let n = BusStripNode::with_channels(w);
                if let Some(v) = v {
                    n.set_pan(tutti_types::Pan(v));
                }
                Box::new(n)
            },
            lo: -1.0,
            hi: 1.0,
        },
    ]
}

/// Frames past the connection's declick.
fn settled(out: &[Vec<f32>]) -> impl Iterator<Item = (usize, usize, f32)> + '_ {
    out.iter().enumerate().flat_map(|(c, o)| {
        o.iter()
            .enumerate()
            .skip(SETTLED)
            .map(move |(i, &x)| (c, i, x))
    })
}

/// **Each param a node's feed declares is the one its DSP reads**, at every
/// width, through the graph: driving the param to `v` sounds like the node
/// with its own control at `v` (within the few ulps a per-frame path and a
/// held one differ by), for two values that sound different from each other.
/// The port of `param_ports.rs`' "the advertised port is the one the DSP
/// reads at every width".
///
/// Mutation (run): in the SVF's `process`, read the fed Q as its cutoff
/// (`feed.get(1, …)` for `feed.get(0, …)`) → "svf cutoff" sounds nothing
/// like a cutoff → fails. Declare `DELAY_PARAMS` in the other order →
/// fails. (A swapped `param_base` is not seen here, since the degenerate
/// range drives the value whatever the base: the node's own
/// `the_feed_declares_*` tests pin the bases.)
#[test]
fn each_fed_param_is_the_one_the_dsp_reads() {
    for w in [1usize, 2, 6] {
        for case in cases() {
            let at = |v: f32| {
                (
                    render((case.make)(w, None), Some((case.param, v))),
                    render((case.make)(w, Some(v)), None),
                )
            };
            let (fed_lo, ctl_lo) = at(case.lo);
            let (fed_hi, ctl_hi) = at(case.hi);
            for (fed, ctl, v) in [(&fed_lo, &ctl_lo, case.lo), (&fed_hi, &ctl_hi, case.hi)] {
                for (c, i, x) in settled(fed) {
                    let want = ctl[c][i];
                    assert!(
                        (x - want).abs() <= 2e-3,
                        "{} (width {w}) fed {v}: channel {c} frame {i} is {x}, the control \
                         at {v} gives {want}",
                        case.name
                    );
                }
            }
            let moved = settled(&fed_lo).any(|(c, i, x)| (x - fed_hi[c][i]).abs() > 1e-2);
            assert!(
                moved,
                "{} (width {w}): {} and {} sound the same",
                case.name, case.lo, case.hi
            );
        }
    }
}

/// A distortion at drive `d` over `x`, as the node renders it: the expected
/// output of a modulated drive, from the unmodulated node.
fn plain_distortion(d: f32) -> Vec<f32> {
    render(
        Box::new(DistortionNode::with_channels(1, ShapeKind::Tanh, d)),
        None,
    )
    .remove(0)
}

/// The base is the node's own control: an authored write to it — through
/// the node's own handle, or a control-rate target mirroring into the same
/// cell — moves the modulated param, ramped across the next block, and the
/// modulation keeps riding on it. There is no second base cell to lose.
///
/// Mutation (run): make `DistortionNode::param_base` answer the drive it
/// was built with (a snapshot, not the cell) → the write never reaches the
/// modulated drive → fails.
#[test]
fn an_authored_write_moves_the_base_under_modulation() {
    let node = DistortionNode::with_channels(1, ShapeKind::Tanh, 1.0);
    let cell = node.drive();
    let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::MONO);
    let n = g.add_unit(Box::new(node));
    let src = g.add_unit(Box::new(Const::mono(0.5)));
    g.connect_input(0, n, 0).connect_output(n, 0, 0);
    g.spec_mut().connect_param(
        ParamIn {
            node: n,
            param: UnitParam::Drive,
        },
        ParamFrom::Audio(OutPort { node: src, port: 0 }),
        ParamShaping::Identity,
    );
    let mut r = g
        .renderer(Prepare::new(SampleRate(48_000.0), Samples(100)))
        .expect("builds");
    let x = noise(7);
    let before = r.render_input(&[&x[..1_000]]).remove(0);
    let at_1_5 = plain_distortion(1.5);
    for i in PARAM_DECLICK.get()..1_000 {
        assert!(
            (before[i] - at_1_5[i]).abs() < 1e-6,
            "frame {i}: base 1.0 + 0.5 is drive 1.5"
        );
    }
    cell.store(3.0, Ordering::Release);
    // One block to ramp there, then the new base under the same offset.
    let after = r.render_input(&[&x[1_000..]]).remove(0);
    let at_3_5 = plain_distortion(3.5);
    for i in 100..after.len() {
        assert!(
            (after[i] - at_3_5[1_000 + i]).abs() < 1e-6,
            "frame {}: the authored 3.0 + 0.5 is drive 3.5",
            1_000 + i
        );
    }
}

/// A crossed range (its min above its max) is ordered, never a panic: a
/// sum past it clamps to the higher bound.
#[test]
fn a_crossed_range_is_survivable() {
    let node = DistortionNode::with_channels(1, ShapeKind::Tanh, 1.0);
    let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::MONO);
    let n = g.add_unit(Box::new(node));
    let src = g.add_unit(Box::new(Const::mono(100.0)));
    g.connect_input(0, n, 0).connect_output(n, 0, 0);
    let at = ParamIn {
        node: n,
        param: UnitParam::Drive,
    };
    g.spec_mut().connect_param(
        at,
        ParamFrom::Audio(OutPort { node: src, port: 0 }),
        ParamShaping::Identity,
    );
    g.spec_mut().set_param_range(at, ParamRange::new(4.0, 2.0));
    let mut r = g
        .renderer(Prepare::new(SampleRate(48_000.0), Samples(100)))
        .expect("builds");
    let out = r.render_input(&[&noise(7)]).remove(0);
    let at_4 = plain_distortion(4.0);
    for i in PARAM_DECLICK.get()..FRAMES {
        assert!((out[i] - at_4[i]).abs() < 1e-6, "frame {i}: clamped at 4");
    }
}

/// An unmodulated param adds nothing to the plan — no op, no slot, no
/// param port — and connecting and disconnecting one is a commit that
/// keeps the unit (the same generation throughout: nothing is rebuilt, so a
/// stateful node's state would survive), with no step reaching the output.
///
/// Mutation (run): in `ParamState::port`, drop the declick (`fading =
/// false`) → the connect steps the drive from 1 to 5 in one frame, a step
/// of 0.6 on a steady tone → fails.
#[test]
fn an_unmodulated_param_adds_nothing_to_the_plan() {
    let node = DistortionNode::with_channels(1, ShapeKind::Tanh, 1.0);
    let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::MONO);
    let n = g.add_unit(Box::new(node));
    let src = g.add_unit(Box::new(Const::mono(4.0)));
    g.connect_input(0, n, 0).connect_output(n, 0, 0);
    let mut r: Renderer = g
        .renderer(Prepare::new(SampleRate(48_000.0), Samples(100)))
        .expect("builds");
    let plan = r.executor().plan().expect("a plan");
    assert!(plan.param_ports().is_empty(), "no param step at all");
    let gen = plan.unit(n).expect("the distortion").gen;

    // A steady tone, so a jump in drive is a jump in level.
    let tone = vec![0.25f32; 600];
    let mut out = r.render_input(&[&tone]).remove(0);
    let at = ParamIn {
        node: n,
        param: UnitParam::Drive,
    };
    let from = ParamFrom::Audio(OutPort { node: src, port: 0 });
    r.editor_mut()
        .spec_mut()
        .connect_param(at, from, ParamShaping::Identity);
    r.editor_mut().commit().expect("connects");
    out.extend(r.render_input(&[&tone]).remove(0));
    r.editor_mut().spec_mut().disconnect_param(at, from);
    r.editor_mut().commit().expect("disconnects");
    out.extend(r.render_input(&[&tone]).remove(0));

    let plan = r.executor().plan().expect("a plan");
    assert_eq!(
        plan.unit(n).expect("the distortion").gen,
        gen,
        "never rebuilt"
    );
    assert!(plan.param_ports().is_empty(), "and nothing left behind");
    // Every frame-to-frame step is bounded by the declick's slope: the drive
    // moves 4 over `PARAM_DECLICK` frames, and `tanh(0.25 d)` moves at most
    // 0.25 per unit of drive.
    let slope = 0.25 * 4.0 / PARAM_DECLICK.get() as f32 + 1e-6;
    for (i, w) in out.windows(2).enumerate() {
        assert!(
            (w[1] - w[0]).abs() <= slope,
            "frame {i}: a step of {}",
            w[1] - w[0]
        );
    }
    // The connection was heard, and undone.
    let (base, fed, back) = (out[599], out[1_199], out[1_799]);
    assert!(fed > base + 0.5, "{base} → {fed}");
    assert_eq!(back, base, "back on the base");
}

/// A fork (an export) modulates as the live graph does: the modulation is
/// part of the graph value it copies, the modulator is upstream of the node
/// it modulates (so it is forked too, even when no output reaches it), and
/// the base is the forked unit's own control. From the fork's first frame:
/// a forked unit's first block is not a change of sources, so nothing fades
/// in.
///
/// Mutation (run): drop `fork.params` from `Editor::fork` → the fork plays
/// the distortion at its base drive → fails. Drop the param sources from
/// `upstream` → the modulator is not forked and the fork refuses to compile
/// the modulation → fails. Declick a port's first block (drop the
/// `st.fresh` branch in `ParamState::port`) → the first frames fade in from
/// drive 1 → fails.
#[test]
fn a_fork_modulates_as_the_live_graph_does() {
    let node = DistortionNode::with_channels(1, ShapeKind::Tanh, 1.0);
    let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::MONO);
    let n = g.add_unit(Box::new(node));
    let src = g.add_unit(Box::new(Const::mono(2.0)));
    g.connect_input(0, n, 0).connect_output(n, 0, 0);
    g.spec_mut().connect_param(
        ParamIn {
            node: n,
            param: UnitParam::Drive,
        },
        ParamFrom::Audio(OutPort { node: src, port: 0 }),
        ParamShaping::Identity,
    );
    let prepare = Prepare::new(SampleRate(48_000.0), Samples(100));
    let (editor, _exec) = g.build(prepare).expect("builds");
    let (fed, fexec) = editor
        .fork(
            tutti_graph::ForkTarget::Master,
            tutti_graph::ForkMode::Live,
            prepare,
        )
        .expect("forks");
    let mut fork = Renderer::new(fed, fexec);
    let x = noise(7);
    let out = fork.render_input(&[&x[..2_000]]).remove(0);
    let at_3 = plain_distortion(3.0);
    for i in 0..2_000 {
        assert!(
            (out[i] - at_3[i]).abs() < 1e-6,
            "frame {i}: the fork plays base 1 + 2, drive 3"
        );
    }
}
