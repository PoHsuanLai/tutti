//! `AutomationLaneNode`'s event output: a curve sent to a param port as
//! curve-segment events (doc 013, decision 7), rendered by the port per
//! frame.
//!
//! What is pinned, each with the mutation it was seen to fail under:
//!
//! - an envelope with non-linear pieces renders its own value at every
//!   frame, not linear pieces between breakpoints, with breakpoints falling
//!   between frames and ragged blocks;
//! - a loop wrap, a seek and a tempo change inside a block, and a stop,
//!   keep the port on the curve;
//! - a piece no segment shape carries goes as linear pieces exact at every
//!   block edge.
//!
//! The figures are `audio-automation`'s own `value_at` at each frame's beat,
//! within a tolerance (the curve's `powf`, `sqrt` and the port's `f32`
//! progress differ in the last bits).

use std::sync::Arc;

use audio_automation::{AutomationEnvelope, AutomationPoint, CurveType};
use tutti_graph::{
    Cx, Editor, EventOut, Executor, Io, Node, Offset, ParamFrom, ParamIn, ParamShaping, Prepare,
    Shape, Status, Transport, TransportChanges, Unforkable,
};
use tutti_mod::Curve;
use tutti_nodes::automation::AutomationLaneNode;
use tutti_types::graph::{OutPort, Source};
use tutti_types::{
    Beat, Bpm, ChannelLayout, FrameClock, Hz, LoopRange, NodeKey, ParamKey, SampleRate, Samples,
    UnitParam,
};

const RATE: f64 = 48_000.0;
/// A tempo whose beat is not a whole number of frames, so breakpoints fall
/// between frames.
const TEMPO: Bpm = Bpm(113.0);
const TOL: f32 = 2e-5;
const MAX: usize = 128;

/// Declares `Cutoff` (base 0) and writes the value it reads, per frame.
struct Probe;

impl Node for Probe {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO).with_params(&[UnitParam::Cutoff])
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        match io.param(0).frames() {
            Some(v) => {
                let v: Vec<f32> = v.to_vec();
                io.output(0).copy_from_slice(&v);
            }
            None => io.output(0).fill(0.0),
        }
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, port: usize) -> Option<f32> {
        (port == 0).then_some(0.0)
    }
}

/// A lane sending `curve` to the probe's cutoff.
fn graph(lane: AutomationLaneNode) -> (Editor, Executor) {
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(RATE), Samples(MAX)));
    ed.insert(NodeKey(1), "lane", lane);
    ed.insert(NodeKey(2), "probe", Unforkable(Probe));
    let s = ed.spec_mut();
    s.topology.outputs = vec![Source::Node(OutPort {
        node: NodeKey(2),
        port: 0,
    })];
    s.connect_param(
        ParamIn {
            node: NodeKey(2),
            param: UnitParam::Cutoff,
        },
        ParamFrom::Events(EventOut {
            node: NodeKey(1),
            port: 0,
        }),
        ParamShaping::Identity,
    );
    ed.commit().expect("commits");
    (ed, exec)
}

fn block(
    ed: &mut Editor,
    exec: &mut Executor,
    t: &Transport,
    ch: &TransportChanges,
    n: usize,
) -> Vec<f32> {
    let mut out = vec![0.0f32; n];
    exec.process_with_changes(n, t, ch, &[], &mut [&mut out[..]]);
    ed.collect();
    out
}

/// A ragged block schedule summing to `total`.
fn ragged(total: usize) -> Vec<usize> {
    let mut v = Vec::new();
    let (mut sum, mut i) = (0, 0usize);
    while sum < total {
        let n = [1, MAX, 7, 64, MAX - 1, 33, 2, 100][i % 8].min(total - sum);
        v.push(n);
        sum += n;
        i += 1;
    }
    v
}

/// Renders `blocks` rolling from beat 0 on the host's clock (looping over
/// `looping` when set), and checks every frame against `curve` at that
/// frame's beat. Returns the rendered values.
fn check_rolling(
    curve: &AutomationEnvelope<()>,
    lane: AutomationLaneNode,
    blocks: &[usize],
    looping: Option<LoopRange>,
) -> Vec<f32> {
    let (mut ed, mut exec) = graph(lane);
    let mut clock = FrameClock::new(Beat(0.0), TEMPO, SampleRate(RATE));
    let graph_loop = looping.map(|l| tutti_graph::LoopRange {
        start: l.start(),
        end: l.end(),
    });
    let mut all = Vec::new();
    for &n in blocks {
        let t = Transport::counted(true, TEMPO, clock.origin(), graph_loop);
        let out = block(&mut ed, &mut exec, &t, &TransportChanges::NONE, n);
        for (i, &v) in out.iter().enumerate() {
            let beat = clock.beat();
            let want = curve.value_at(beat).expect("a value");
            assert!(
                (v - want).abs() <= TOL,
                "frame {} (block of {n}, frame {i}), beat {}: {v} vs the curve's {want}",
                all.len() + i,
                beat.get()
            );
            clock.advance(Samples(1), looping);
        }
        all.extend(out);
    }
    all
}

fn frames_per_beat() -> f64 {
    RATE * 60.0 / TEMPO.get()
}

/// Exponential (`t²`), cubic-out, S-curve and linear pieces, the
/// breakpoints between frames.
fn shaped() -> AutomationEnvelope<()> {
    let mut e = AutomationEnvelope::new(());
    e.add_point(AutomationPoint::with_curve(
        0.0,
        0.0,
        CurveType::Exponential,
    ));
    e.add_point(AutomationPoint::with_curve(0.25, 1.0, CurveType::CubicOut));
    e.add_point(AutomationPoint::with_curve(0.4, 0.2, CurveType::SCurve));
    e.add_point(AutomationPoint::with_curve(0.55, 0.9, CurveType::Linear));
    e.add_point(AutomationPoint::new(0.6, 0.4));
    e
}

/// An envelope of exponential and other non-linear pieces renders the
/// curve at every frame, through breakpoints between frames, under ragged
/// blocks: where linear pieces and the curve differ (halfway along the
/// exponential piece, a quarter for a half), the port reads the curve.
///
/// Mutations (run), each fails: `segment_shape` mapping `Exponential` to
/// `Linear`; the lane sending every piece as the piecewise-linear fallback
/// (ignoring `segment_at`); the lane computing a segment's position from
/// the piece's end instead of its start.
#[test]
fn an_exponential_lane_renders_the_curve_not_linear_pieces() {
    let curve = shaped();
    let total = (0.7 * frames_per_beat()) as usize;
    for blocks in [ragged(total), vec![MAX; total / MAX]] {
        let out = check_rolling(
            &curve,
            AutomationLaneNode::for_param(curve.clone(), ParamKey::<Hz>::CUTOFF),
            &blocks,
            None,
        );
        // Halfway along the exponential piece (beat 0.125): a quarter of
        // the way up, where a line between its breakpoints is at half.
        let mid = (0.125 * frames_per_beat()).round() as usize;
        assert!((out[mid] - 0.25).abs() < 1e-3, "{}", out[mid]);
        assert!((out[mid] - 0.5).abs() > 0.2, "not linear: {}", out[mid]);
        // Held after the last point.
        assert_eq!(*out.last().unwrap(), 0.4);
    }
}

/// A loop wrapping inside blocks, then a seek, a tempo change and a stop
/// landing mid-block, keep the port on the curve at every frame: the lane
/// re-sends the piece it is on wherever the beat jumps.
///
/// Mutations (run), each fails: the lane not re-sending on a wrap (`wrapped`
/// ignored); the lane re-sending only at a block's first piece (the
/// transport changes' pieces skipped).
#[test]
fn wraps_seeks_and_tempo_changes_stay_on_the_curve() {
    let curve = shaped();
    // Loop 0.1 → 0.5 beats, through four pieces; ragged blocks wrap inside.
    let looping = LoopRange::new(Beat(0.1), Beat(0.5));
    let total = (1.3 * frames_per_beat()) as usize;
    check_rolling(
        &curve,
        AutomationLaneNode::for_param(curve.clone(), ParamKey::<Hz>::CUTOFF),
        &ragged(total),
        looping,
    );

    // A seek, then a tempo change, then a stop, each at offset 50 of a
    // 128-frame block.
    let (mut ed, mut exec) = graph(AutomationLaneNode::for_param(
        curve.clone(),
        ParamKey::<Hz>::CUTOFF,
    ));
    let at50 = Offset::new(50, Samples(MAX)).expect("inside");
    let cases: [(Transport, Transport); 3] = [
        (
            Transport::new(true, TEMPO, Beat(0.05), None),
            Transport::new(true, TEMPO, Beat(0.3), None),
        ),
        (
            Transport::new(true, TEMPO, Beat(0.12), None),
            Transport::new(true, Bpm(71.0), Beat(0.12 + 50.0 / frames_per_beat()), None),
        ),
        (
            Transport::new(true, TEMPO, Beat(0.45), None),
            Transport::new(false, TEMPO, Beat(0.45 + 50.0 / frames_per_beat()), None),
        ),
    ];
    for (k, (t0, t1)) in cases.into_iter().enumerate() {
        let mut ch = TransportChanges::NONE;
        ch.push(at50, t1).expect("room");
        let out = block(&mut ed, &mut exec, &t0, &ch, MAX);
        let fpb = |t: &Transport| RATE * 60.0 / t.tempo.get();
        for (i, &v) in out.iter().enumerate() {
            let beat = if i < 50 {
                t0.beat().get() + i as f64 / fpb(&t0)
            } else if t1.playing {
                t1.beat().get() + (i - 50) as f64 / fpb(&t1)
            } else {
                t1.beat().get()
            };
            let want = curve.value_at(Beat(beat)).expect("a value");
            assert!(
                (v - want).abs() <= 1e-4,
                "case {k}, frame {i}, beat {beat}: {v} vs {want}"
            );
        }
    }
}

/// A piece whose shape no segment carries (`Elastic` overshoots) goes as
/// linear pieces between the curve's values at every block edge: exact on
/// each block's first and last frame, and a line between them.
///
/// Mutations (run), each fails: the fallback sending nothing (the port
/// holds its last value); the fallback's line ending one frame early
/// (`e - i` as its length less one).
#[test]
fn an_unrepresentable_piece_is_linear_between_block_edges() {
    let mut curve = AutomationEnvelope::new(());
    curve.add_point(AutomationPoint::with_curve(0.0, 0.0, CurveType::Elastic));
    curve.add_point(AutomationPoint::new(0.2, 1.0));
    let (mut ed, mut exec) = graph(AutomationLaneNode::for_param(
        curve.clone(),
        ParamKey::<Hz>::CUTOFF,
    ));
    let mut clock = FrameClock::new(Beat(0.0), TEMPO, SampleRate(RATE));
    for &n in &ragged((0.15 * frames_per_beat()) as usize) {
        let t = Transport::counted(true, TEMPO, clock.origin(), None);
        let out = block(&mut ed, &mut exec, &t, &TransportChanges::NONE, n);
        let at = |i: usize| {
            let mut c = clock;
            c.advance(Samples(i), None);
            curve.value_at(c.beat()).expect("a value")
        };
        let (v0, v1) = (at(0), at(n - 1));
        for (i, &v) in out.iter().enumerate() {
            let want = if n == 1 {
                v0
            } else {
                v0 + (v1 - v0) * (i as f32 / (n - 1) as f32)
            };
            assert!((v - want).abs() <= 1e-4, "frame {i} of {n}: {v} vs {want}");
        }
        clock.advance(Samples(n), None);
    }
}

/// A curve that cannot say its pieces (`segment_at` is `None`: here a
/// sine) takes the same fallback: exact on each block's first and last
/// frame.
///
/// Mutation (run): the fallback sending nothing → the port holds 0 → fails.
#[test]
fn a_curve_without_pieces_takes_the_fallback() {
    struct Sine;
    impl Curve for Sine {
        fn value_at(&self, beat: Beat) -> Option<f32> {
            Some((beat.get() as f32 * 3.0).sin())
        }
    }
    let curve: Arc<dyn Curve> = Arc::new(Sine);
    let (mut ed, mut exec) = graph(AutomationLaneNode::for_param(Sine, ParamKey::<Hz>::CUTOFF));
    let mut clock = FrameClock::new(Beat(0.0), TEMPO, SampleRate(RATE));
    for _ in 0..20 {
        let t = Transport::counted(true, TEMPO, clock.origin(), None);
        let out = block(&mut ed, &mut exec, &t, &TransportChanges::NONE, 64);
        let want = curve.value_at(clock.beat()).expect("a value");
        assert!((out[0] - want).abs() <= 1e-5, "{} vs {want}", out[0]);
        clock.advance(Samples(63), None);
        let want = curve.value_at(clock.beat()).expect("a value");
        assert!((out[63] - want).abs() <= 1e-5, "{} vs {want}", out[63]);
        clock.advance(Samples(1), None);
    }
}
