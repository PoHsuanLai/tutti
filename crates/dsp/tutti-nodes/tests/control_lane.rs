//! `ControlLaneNode` (doc 014 §5): a target's `ControlProgram` evaluated on
//! the play-clock grid and sent as segments, against analytic figures and
//! invariants of the render (never another reference render).
//!
//! Each test names the mutations it was seen to fail under. The 200-lane
//! serial == parallel and no-allocation gates are in
//! `rt_no_alloc_modulation.rs`, beside the gated pool they share.

use std::sync::Arc;

use audio_automation::{AutomationEnvelope, AutomationPoint};
use tutti_graph::{
    Cx, Editor, Event, EventEdge, EventIn, EventKind, EventOut, Executor, ForkByClone, ForkMode,
    ForkTarget, Io, Node, Offset, ParamIn, ParamRange, ParamSegment, Prepare, SegmentShape, Shape,
    Status, Transport, TransportChanges, PARAM_DECLICK,
};
use tutti_mod::{
    Amount, Clock, ControlProgram, Curve, CurveType, DepthBy, InputLayer, Layer, LayerId, LfoShape,
    PeriodPoint, Polarity, RateAnchor, SourceId, Wave,
};
use tutti_nodes::automation::{ControlLaneNode, LaneControls, CONTROL_STEP};
use tutti_types::graph::{OutPort, Source};
use tutti_types::{
    AtomicF32, Beat, BeatDuration, Bpm, ChannelLayout, Depth, Frame, FrameClock, Hz, NodeKey,
    ParamKey, Phase, PhaseIncrement, SampleRate, Samples, UnitParam,
};

const RATE: f64 = 48_000.0;
/// 24 000 frames a beat: beat figures land on whole frames.
const TEMPO: Bpm = Bpm(120.0);
const FPB: f64 = 24_000.0;
const STEP: usize = CONTROL_STEP as usize;

// ---------------------------------------------------------------- nodes --

/// Declares `Cutoff` (base 0) and writes the value its port reads, per
/// frame.
#[derive(Clone)]
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

/// A plugin's view of a lane: takes the segments on its event input as a
/// hosted plugin's delivery does (`ParamSegment::raw_value_after` from each
/// event's frame) and writes the value per frame on output 0, and on output
/// 1 a mark on each event's frame: 1 for a `Linear` segment, 2 for any
/// other shape.
#[derive(Clone, Default)]
struct SegSink {
    last: Option<(ParamSegment, u64)>,
}

impl Node for SegSink {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::STEREO).with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let n = io.frames();
        let base = cx.env.frame.get();
        let events: Vec<(usize, ParamSegment)> = io
            .events(0)
            .as_slice()
            .iter()
            .filter_map(|e| match e.kind {
                EventKind::Segment(s) => Some((e.offset.index(), s)),
                _ => None,
            })
            .collect();
        let (mut vals, mut marks) = (vec![0.0f32; n], vec![0.0f32; n]);
        let mut k = 0;
        for i in 0..n {
            while let Some(&(_, s)) = events.get(k).filter(|e| e.0 == i) {
                self.last = Some((s, base + i as u64));
                marks[i] = if s.shape() == SegmentShape::Linear {
                    1.0
                } else {
                    2.0
                };
                k += 1;
            }
            vals[i] = self.last.map_or(0.0, |(s, at)| {
                s.raw_value_after(Samples((base + i as u64 - at) as usize))
            });
        }
        io.output(0).copy_from_slice(&vals);
        io.output(1).copy_from_slice(&marks);
        Status::Modified
    }
    fn reset(&mut self) {
        self.last = None;
    }
}

/// A signal source: sends each planned segment, addressed to a foreign id
/// (a lane's input source), on its frame.
#[derive(Clone)]
struct Feed {
    plan: Vec<(u64, ParamSegment)>,
}

impl Node for Feed {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY).with_events(0, 1)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (base, n) = (cx.env.frame.get(), cx.env.block_len);
        for &(at, seg) in &self.plan {
            if let Some(o) = at
                .checked_sub(base)
                .and_then(|o| Offset::new(o as usize, n))
            {
                io.event_out(0).push(Event::segment(o, seg)).expect("room");
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

// ---------------------------------------------------------------- rigs ---

fn cutoff(node: u64) -> ParamIn {
    ParamIn {
        node: NodeKey(node),
        param: UnitParam::Cutoff,
    }
}

fn out(node: u64, port: u16) -> Source {
    Source::Node(OutPort {
        node: NodeKey(node),
        port,
    })
}

/// An editor with a probe at each of `probes` (cutoff ranged `range`), its
/// outputs the probes' in order; committed, so the range cells exist.
fn probes(max: usize, probes: &[u64], range: (f32, f32)) -> (Editor, Executor) {
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(RATE), Samples(max)));
    for &p in probes {
        ed.insert(NodeKey(p), "probe", ForkByClone(Probe));
        ed.spec_mut().topology.outputs.push(out(p, 0));
    }
    for &p in probes {
        ed.set_param_range(cutoff(p), ParamRange::new(range.0, range.1))
            .expect("a probe");
    }
    ed.commit().expect("commits");
    (ed, exec)
}

/// A native lane for `probe`'s cutoff: knob `knob` (its own cell, as a
/// node's `Param` cell would be), the port's own range cell.
fn native(ed: &Editor, probe: u64, program: ControlProgram, knob: Option<f32>) -> ControlLaneNode {
    ControlLaneNode::for_param(
        ParamKey::<Hz>::CUTOFF,
        program,
        knob.map(|k| Arc::new(AtomicF32::new(k))),
        Some(ed.param_range_cell(cutoff(probe)).expect("committed")),
    )
}

/// Inserts `lane` at `key`, wired to `probe`'s cutoff; returns its controls.
fn wire(ed: &mut Editor, key: u64, lane: ControlLaneNode, probe: u64) -> LaneControls {
    let controls = ed.insert(NodeKey(key), "lane", lane);
    ed.spec_mut().connect_param(
        cutoff(probe),
        EventOut {
            node: NodeKey(key),
            port: 0,
        },
    );
    controls
}

/// Inserts a `SegSink` at `sink` fed by `lane` at `key`, its two outputs
/// appended to the graph's.
fn wire_sink(ed: &mut Editor, key: u64, lane: ControlLaneNode, sink: u64) -> LaneControls {
    let controls = ed.insert(NodeKey(key), "lane", lane);
    ed.insert(NodeKey(sink), "sink", ForkByClone(SegSink::default()));
    let s = ed.spec_mut();
    s.connect_events(
        EventIn {
            node: NodeKey(sink),
            port: 0,
        },
        EventEdge::Direct(EventOut {
            node: NodeKey(key),
            port: 0,
        }),
    );
    s.topology.outputs.extend([out(sink, 0), out(sink, 1)]);
    controls
}

/// One block of `n` frames: every output channel's frames.
fn block(
    ed: &mut Editor,
    exec: &mut Executor,
    t: &Transport,
    ch: &TransportChanges,
    n: usize,
) -> Vec<Vec<f32>> {
    let outs = ed.spec().topology.outputs.len();
    let mut bufs = vec![vec![0.0f32; n]; outs];
    {
        let mut refs: Vec<&mut [f32]> = bufs.iter_mut().map(|b| &mut b[..]).collect();
        exec.process_with_changes(n, t, ch, &[], &mut refs);
    }
    ed.collect();
    bufs
}

/// The host's playhead: a frame clock from beat 0 at `TEMPO`.
struct Host {
    clock: FrameClock,
}

impl Host {
    fn new() -> Self {
        Self {
            clock: FrameClock::new(Beat(0.0), TEMPO, SampleRate(RATE)),
        }
    }

    fn rolling(&self) -> Transport {
        Transport::counted(true, TEMPO, self.clock.origin(), None)
    }

    /// One rolling block of `n`, appended channel by channel to `all`.
    fn roll(&mut self, ed: &mut Editor, exec: &mut Executor, n: usize, all: &mut Vec<Vec<f32>>) {
        let t = self.rolling();
        let got = block(ed, exec, &t, &TransportChanges::NONE, n);
        append(all, got);
        self.clock.advance(Samples(n), None);
    }
}

fn append(all: &mut Vec<Vec<f32>>, got: Vec<Vec<f32>>) {
    all.resize(got.len().max(all.len()), Vec::new());
    for (a, g) in all.iter_mut().zip(got) {
        a.extend(g);
    }
}

/// A ragged block schedule up to `max`, summing to `total`.
fn ragged(total: usize, max: usize) -> Vec<usize> {
    let mut v = Vec::new();
    let (mut sum, mut i) = (0, 0usize);
    while sum < total {
        let n = [1, max, 7, 64, max - 1, 33, 2, 100][i % 8]
            .min(max)
            .min(total - sum);
        v.push(n);
        sum += n;
        i += 1;
    }
    v
}

// ------------------------------------------------------------- programs --

fn amount(depth: f32) -> Amount {
    Amount {
        depth: Depth(depth),
        polarity: Polarity::Bipolar,
        curve: CurveType::Linear,
    }
}

fn beats(per_cycle: f64) -> Clock {
    Clock::Beats {
        per_cycle: BeatDuration(per_cycle),
        phase: PhaseIncrement(0.0),
    }
}

fn seconds(rate: f32) -> Clock {
    Clock::Seconds {
        rate: Hz(rate),
        phase: PhaseIncrement(0.0),
        anchor: None,
    }
}

fn wave(shape: LfoShape) -> Wave {
    Wave::Shape { shape, seed: 0 }
}

fn layer(id: u64, clock: Clock, wave: Wave, amount: Amount) -> Layer {
    Layer {
        id: LayerId(id),
        clock,
        wave,
        amount,
        depth_by: None,
    }
}

fn program(automation: Option<Arc<dyn Curve>>, layers: Vec<Layer>) -> ControlProgram {
    ControlProgram::new(automation, layers, vec![]).expect("a valid program")
}

/// A linear envelope through `points` (beat, value).
fn envelope(points: &[(f64, f32)]) -> AutomationEnvelope<()> {
    let mut e = AutomationEnvelope::new(());
    for &(b, v) in points {
        e.add_point(AutomationPoint::new(b, v));
    }
    e
}

/// The triangle `LfoShape::Triangle` draws, in `f64`.
fn triangle(cycles: f64) -> f64 {
    let p = cycles.rem_euclid(1.0) * 4.0;
    if p < 1.0 {
        p
    } else if p < 3.0 {
        2.0 - p
    } else {
        p - 4.0
    }
}

// ---------------------------------------------------------------- tests --

/// Automation replaces the knob, offsets add to it, and the clamp is per
/// frame (doc 014 §2, §12's figures):
///
/// - knob 1000 under a constant 500 automation reads 500.0 on every frame,
///   on a native port (exact mode) and on a plugin lane (the program's own
///   evaluation: a plugin lane never takes exact mode);
/// - a ±100 bar-locked square (depth 0.125 of a 100..900 range) on knob 500
///   reads 600.0 for the first two beats and 400.0 for the next two, its
///   edge on its own frame (48 000) inside a block;
/// - a linear 0 → 10 piece over one beat under a bound of 5.0625 meets it
///   at frame 12 150, off the grid (12 150 = 379·32 + 22), and holds it: on
///   a native port (exact mode; the port clamps) and on a plugin lane, which
///   clamps its own pieces and splits the grid piece at the crossing.
///
/// Mutations (run), each fails: the base as `knob + automation`
/// (`ControlProgram::evaluate`) → the plugin lane reads 1500; the layers'
/// offsets divided by the span (a span of 1) → 500.125; the plugin lane's
/// clamp dropped (`clamp` returns `v`) → past 5.0625; the split dropped
/// (`if let Some(..) = cross` made dead) → frame 12 150 reads 5.0596 on the
/// unsplit line; the split's `f32` tolerance dropped → this crossing, on
/// frame 22 of its piece, computes a hair past it and splits a frame late
/// (frame 12 134 reads 5.05572 against 5.05583).
#[test]
fn automation_replaces_the_knob_and_the_clamp_is_per_frame() {
    // Knob 1000, automation 500.
    let (mut ed, mut exec) = probes(128, &[2], (0.0, 2000.0));
    let flat: Arc<dyn Curve> = Arc::new(envelope(&[(0.0, 500.0)]));
    let lane = native(
        &ed,
        2,
        program(Some(Arc::clone(&flat)), vec![]),
        Some(1000.0),
    );
    wire(&mut ed, 1, lane, 2);
    let plugin =
        ControlLaneNode::for_foreign(9, program(Some(flat), vec![]), Some(1000.0), (0.0, 2000.0));
    wire_sink(&mut ed, 3, plugin, 4);
    ed.commit().expect("commits");
    let (mut host, mut all) = (Host::new(), Vec::new());
    for _ in 0..20 {
        host.roll(&mut ed, &mut exec, 128, &mut all);
    }
    for (name, v) in [("native", &all[0]), ("plugin", &all[1])] {
        assert!(
            v.iter().all(|&v| v == 500.0),
            "{name}: the automation, not the knob"
        );
    }

    // A ±100 square on knob 500.
    let (mut ed, mut exec) = probes(512, &[2], (100.0, 900.0));
    let square = layer(1, beats(4.0), wave(LfoShape::Square), amount(0.125));
    let lane = native(&ed, 2, program(None, vec![square]), Some(500.0));
    wire(&mut ed, 1, lane, 2);
    ed.commit().expect("commits");
    let (mut host, mut all) = (Host::new(), Vec::new());
    while all.first().map_or(0, Vec::len) < 96_000 {
        host.roll(&mut ed, &mut exec, 448, &mut all);
    }
    for (f, &v) in all[0][..96_000].iter().enumerate() {
        let want = if f < 48_000 { 600.0 } else { 400.0 };
        assert_eq!(v, want, "frame {f}");
    }

    // A clamped ramp, native and plugin.
    const BOUND: f32 = 5.0625;
    let ramp = || -> Arc<dyn Curve> { Arc::new(envelope(&[(0.0, 0.0), (1.0, 10.0)])) };
    let (mut ed, mut exec) = probes(128, &[2], (0.0, BOUND));
    let lane = native(&ed, 2, program(Some(ramp()), vec![]), Some(0.0));
    wire(&mut ed, 1, lane, 2);
    let plugin = ControlLaneNode::for_foreign(9, program(Some(ramp()), vec![]), None, (0.0, BOUND));
    wire_sink(&mut ed, 3, plugin, 4);
    ed.commit().expect("commits");
    let (mut host, mut all) = (Host::new(), Vec::new());
    for _ in 0..200 {
        host.roll(&mut ed, &mut exec, 128, &mut all);
    }
    for (name, v) in [("native", &all[0]), ("plugin", &all[1])] {
        for (f, &got) in v.iter().enumerate() {
            let line = 10.0 * f as f64 / FPB;
            let want = line.min(f64::from(BOUND));
            assert!(got <= BOUND, "{name}, frame {f}: {got} past the bound");
            assert!(
                (f64::from(got) - want).abs() <= 1e-4,
                "{name}, frame {f}: {got} vs {want}"
            );
            if f > 12_150 {
                assert_eq!(got, BOUND, "{name}, frame {f}: held at the bound");
            }
        }
    }
}

/// An export (a fork of the live graph rendered in 1024-frame blocks) is the
/// live render, bit for bit, from the play start on: for every layer kind
/// (beats, seconds, drawn, random and smooth random, `depth_by`, automation
/// in exact mode, automation under a layer) on native ports, and a plugin
/// lane's points. Live runs stopped first, then starts inside a block on
/// frame 421 (not a grid multiple), and renders ragged blocks.
///
/// Mutations (run), each fails: a lane re-sending at every block's first
/// frame (`due` always true) → pieces restart at live's block edges; the
/// grid counted from the block's first frame (`grid = stride - p % stride`)
/// → the grid moves with the blocks; the grid on the executor's frame
/// rather than the play clock (`grid` from `abs`) → live's grid is 421
/// frames off the export's.
#[test]
fn an_export_is_the_live_render_bit_for_bit() {
    let lanes: Vec<ControlProgram> = {
        let shaped: Arc<dyn Curve> = {
            let mut e = AutomationEnvelope::new(());
            e.add_point(AutomationPoint::with_curve(
                0.0,
                300.0,
                CurveType::Exponential,
            ));
            e.add_point(AutomationPoint::with_curve(0.37, 1500.0, CurveType::SCurve));
            e.add_point(AutomationPoint::with_curve(1.1, 900.0, CurveType::Elastic));
            e.add_point(AutomationPoint::new(1.9, 600.0));
            Arc::new(e)
        };
        let drawn = Wave::Drawn(Arc::from(vec![
            PeriodPoint {
                at: Phase(0.0),
                value: -1.0,
                curve: CurveType::Linear,
            },
            PeriodPoint {
                at: Phase(0.3),
                value: 0.8,
                curve: CurveType::Exponential,
            },
            PeriodPoint {
                at: Phase(0.7),
                value: 0.2,
                curve: CurveType::SCurve,
            },
        ]));
        let curved = Amount {
            curve: CurveType::Exponential,
            ..amount(0.2)
        };
        vec![
            program(
                None,
                vec![layer(1, beats(1.5), wave(LfoShape::Sine), amount(0.3))],
            ),
            program(
                None,
                vec![layer(1, seconds(3.7), wave(LfoShape::Triangle), curved)],
            ),
            program(None, vec![layer(1, beats(0.75), drawn, amount(0.25))]),
            program(
                None,
                vec![
                    layer(
                        1,
                        beats(0.5),
                        Wave::Shape {
                            shape: LfoShape::Random,
                            seed: 9,
                        },
                        amount(0.1),
                    ),
                    layer(
                        2,
                        seconds(5.0),
                        Wave::Shape {
                            shape: LfoShape::RandomSmooth,
                            seed: 3,
                        },
                        amount(0.1),
                    ),
                ],
            ),
            program(
                None,
                vec![Layer {
                    depth_by: Some(DepthBy {
                        clock: beats(1.0),
                        wave: wave(LfoShape::Square),
                        amount: DepthBy::UNIT,
                    }),
                    ..layer(1, seconds(2.0), wave(LfoShape::Sine), amount(0.3))
                }],
            ),
            program(Some(Arc::clone(&shaped)), vec![]),
            program(
                Some(shaped),
                vec![layer(
                    1,
                    seconds(7.0),
                    wave(LfoShape::Sawtooth),
                    amount(0.05),
                )],
            ),
        ]
    };
    let keys: Vec<u64> = (0..lanes.len() as u64).map(|k| 100 + k).collect();
    let (mut ed, mut exec) = probes(128, &keys, (20.0, 2000.0));
    for (k, p) in lanes.into_iter().enumerate() {
        let lane = native(&ed, keys[k], p, Some(440.0));
        wire(&mut ed, 200 + k as u64, lane, keys[k]);
    }
    let foreign = program(
        None,
        vec![layer(1, seconds(4.3), wave(LfoShape::Sine), amount(0.4))],
    );
    wire_sink(
        &mut ed,
        300,
        ControlLaneNode::for_foreign(5, foreign, Some(0.5), (0.0, 1.0)),
        301,
    );
    ed.commit().expect("commits");

    // Three stopped blocks, then the fork, then live starts at offset 37.
    let stopped = Transport::new(false, TEMPO, Beat(0.0), None);
    for _ in 0..3 {
        block(&mut ed, &mut exec, &stopped, &TransportChanges::NONE, 128);
    }
    let (mut fed, mut fexec) = ed
        .fork(
            ForkTarget::Master,
            ForkMode::Offline,
            Prepare::new(SampleRate(RATE), Samples(1024)),
        )
        .expect("forks");
    let total = 60_000;

    let mut host = Host::new();
    let mut ch = TransportChanges::NONE;
    ch.push(
        Offset::new(37, Samples(128)).expect("inside"),
        host.rolling(),
    )
    .expect("room");
    let mut live = block(&mut ed, &mut exec, &stopped, &ch, 128);
    for c in &mut live {
        c.drain(..37);
    }
    host.clock.advance(Samples(128 - 37), None);
    for n in ragged(total - (128 - 37), 128) {
        host.roll(&mut ed, &mut exec, n, &mut live);
    }

    let (mut host, mut export) = (Host::new(), Vec::new());
    while export.first().map_or(0, Vec::len) < total {
        host.roll(&mut fed, &mut fexec, 1024, &mut export);
    }
    for (c, (l, e)) in live.iter().zip(&export).enumerate() {
        assert!(
            l.iter().any(|&v| v != l[0]),
            "channel {c} moves: the comparison walks pieces"
        );
        for f in 0..total {
            assert_eq!(
                l[f].to_bits(),
                e[f].to_bits(),
                "channel {c}, frame {f} of play: live {} vs export {}",
                l[f],
                e[f]
            );
        }
    }
}

/// One LFO on two targets is phase-coherent: a second lane with the same
/// program, wired in later at a frame off the grid, renders the first's
/// bits from its first grid point after its connect crossfade (the port
/// fades in from its base over `PARAM_DECLICK`, doc 014 §5.6).
///
/// Mutation (run): a point's next point one step on (`grid` as
/// `self.stride`) rather than at the next grid multiple → the late lane's
/// points sit off the first's → fails.
#[test]
fn one_lfo_on_two_targets_is_phase_coherent() {
    let p = || {
        program(
            None,
            vec![
                layer(1, seconds(3.0), wave(LfoShape::Sine), amount(0.2)),
                layer(2, beats(0.5), wave(LfoShape::Triangle), amount(0.1)),
            ],
        )
    };
    let (mut ed, mut exec) = probes(128, &[2, 3], (0.0, 2000.0));
    let first = native(&ed, 2, p(), Some(1000.0));
    wire(&mut ed, 10, first, 2);
    ed.commit().expect("commits");
    let (mut host, mut all) = (Host::new(), Vec::new());
    for n in ragged(5_000, 128) {
        host.roll(&mut ed, &mut exec, n, &mut all);
    }
    let joined = all[0].len();
    assert_ne!(joined % STEP, 0, "joins off the grid");
    let second = native(&ed, 3, p(), Some(1000.0));
    wire(&mut ed, 11, second, 3);
    ed.commit().expect("commits");
    let blocks = ragged(20_000, 128);
    host.roll(&mut ed, &mut exec, blocks[0], &mut all);
    let from = (joined + PARAM_DECLICK.get()).next_multiple_of(STEP);
    for &n in &blocks[1..] {
        host.roll(&mut ed, &mut exec, n, &mut all);
    }
    for (f, (a, b)) in all[0].iter().zip(&all[1]).enumerate().skip(from) {
        assert_eq!(
            a.to_bits(),
            b.to_bits(),
            "frame {f}: {} vs {}, joined {joined}, from {from}",
            a,
            b
        );
    }
}

/// A rate edit through an anchor (doc 014 §4.3): two lanes that read it on
/// different blocks, both before the anchor, are phase-identical at every
/// frame before, at and after it; the triangle follows 3 Hz up to the
/// anchor and 5 Hz from it, continuously, with no step larger than one
/// frame's slope; and after a stop and a play start (a new epoch) the stale
/// anchor is ignored: the layer runs at 5 Hz from phase 0.
///
/// Mutations (run), each fails: the anchor ignored (`cycles` never takes
/// it) → the phase jumps at the anchor; a stale anchor honoured (the epoch
/// check dropped) → after the restart the phase is the anchor's; the
/// anchor's frame not a corner (`next_corner` without it) → the piece
/// across it misses the kink by more than the tolerance.
#[test]
fn a_rate_anchor_holds_two_lanes_in_phase() {
    let tri = |clock| {
        program(
            None,
            vec![layer(1, clock, wave(LfoShape::Triangle), amount(0.25))],
        )
    };
    let (mut ed, mut exec) = probes(128, &[2, 3], (100.0, 900.0));
    let a = native(&ed, 2, tri(seconds(3.0)), Some(500.0));
    let b = native(&ed, 3, tri(seconds(3.0)), Some(500.0));
    let (ca, cb) = (wire(&mut ed, 10, a, 2), wire(&mut ed, 11, b, 3));
    ed.commit().expect("commits");
    let (mut host, mut all) = (Host::new(), Vec::new());
    const AT: u64 = 4_010;
    let anchored = |epoch: u32| {
        tri(Clock::Seconds {
            rate: Hz(5.0),
            phase: PhaseIncrement(0.0),
            anchor: Some(RateAnchor {
                epoch,
                at: Frame(AT),
                phase: 3.0 * AT as f64 / RATE,
                prev_rate: Hz(3.0),
            }),
        })
    };
    for k in 0..100 {
        if k == 20 {
            ca.publish(anchored(ed.play_reading().epoch));
        }
        if k == 25 {
            cb.publish(anchored(ed.play_reading().epoch));
        }
        host.roll(&mut ed, &mut exec, 128, &mut all);
    }
    let anchor_cycles = 3.0 * AT as f64 / RATE;
    let want = |f: usize| {
        let c = if (f as u64) < AT {
            3.0 * f as f64 / RATE
        } else {
            anchor_cycles + 5.0 * (f as u64 - AT) as f64 / RATE
        };
        500.0 + 200.0 * triangle(c)
    };
    let slope = 4.0 * 5.0 / RATE * 200.0;
    for f in 0..all[0].len() {
        let (a, b) = (f64::from(all[0][f]), f64::from(all[1][f]));
        assert!(
            (a - want(f)).abs() <= 1e-3,
            "a, frame {f}: {a} vs {}",
            want(f)
        );
        assert!(
            (b - want(f)).abs() <= 1e-3,
            "b, frame {f}: {b} vs {}",
            want(f)
        );
        if f > 0 {
            let step = (a - f64::from(all[0][f - 1])).abs();
            assert!(step <= slope * 1.001, "frame {f}: a step of {step}");
        }
    }

    // Stop, then start again: a new epoch, the anchor stale.
    let stopped = Transport::new(false, TEMPO, Beat(0.0), None);
    block(&mut ed, &mut exec, &stopped, &TransportChanges::NONE, 128);
    let mut host = Host::new();
    let mut again = Vec::new();
    for _ in 0..40 {
        host.roll(&mut ed, &mut exec, 128, &mut again);
    }
    for (f, &v) in again[0].iter().enumerate() {
        let want = 500.0 + 200.0 * triangle(5.0 * f as f64 / RATE);
        assert!(
            (f64::from(v) - want).abs() <= 1e-3,
            "after the restart, frame {f}: {v} vs {want}"
        );
    }
}

/// A range edit rescales an offset from the next block with no republish
/// (doc 014 §4.2): a ±100 square (depth 0.125 of 100..900) on knob 500
/// reads 600; the range moves to 100..700 between blocks, and the lane
/// glides from 600 to 575 (0.125 of 600) by the next grid point and holds
/// it.
///
/// Mutations (run), each fails: the span read once when the lane is built
/// → still 600; the range left out of `changed` → no glide (600 held to the
/// grid point, then a step).
#[test]
fn a_range_edit_rescales_on_the_next_block() {
    let (mut ed, mut exec) = probes(128, &[2], (100.0, 900.0));
    let square = layer(1, beats(4.0), wave(LfoShape::Square), amount(0.125));
    let lane = native(&ed, 2, program(None, vec![square]), Some(500.0));
    wire(&mut ed, 1, lane, 2);
    ed.commit().expect("commits");
    let (mut host, mut all) = (Host::new(), Vec::new());
    for _ in 0..10 {
        host.roll(&mut ed, &mut exec, 100, &mut all);
    }
    assert!(all[0].iter().all(|&v| v == 600.0));
    ed.set_param_range(cutoff(2), ParamRange::new(100.0, 700.0))
        .expect("a probe");
    let edit = all[0].len();
    assert_ne!(edit % STEP, 0, "the edit lands off the grid");
    for _ in 0..10 {
        host.roll(&mut ed, &mut exec, 100, &mut all);
    }
    let grid = edit.next_multiple_of(STEP);
    assert_eq!(
        all[0][edit], 600.0,
        "the glide starts from the value in flight"
    );
    for (f, &v) in all[0].iter().enumerate().take(grid).skip(edit + 1) {
        assert!(v < 600.0 && v > 575.0, "frame {f}: {v}, gliding");
    }
    for (f, &v) in all[0].iter().enumerate().skip(grid) {
        assert_eq!(v, 575.0, "frame {f}");
    }
}

/// A plugin lane under shaped automation sends linear grid pieces, never
/// exact mode (doc 014 §5.4): every segment it sends is `Linear`, a point
/// lands on every grid frame, and the value there is the curve's.
///
/// Mutation (run): `for_foreign` building with `native: true` → the
/// exponential piece goes as one `Exponential` segment → fails.
#[test]
fn a_plugin_lane_sends_linear_grid_pieces() {
    let mut e = AutomationEnvelope::new(());
    e.add_point(AutomationPoint::with_curve(
        0.0,
        0.1,
        CurveType::Exponential,
    ));
    e.add_point(AutomationPoint::with_curve(0.3, 0.9, CurveType::SCurve));
    e.add_point(AutomationPoint::new(0.6, 0.4));
    let curve: Arc<dyn Curve> = Arc::new(e);
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(RATE), Samples(128)));
    let lane = ControlLaneNode::for_foreign(
        3,
        ControlProgram::from_automation(Arc::clone(&curve)),
        None,
        (0.0, 1.0),
    );
    wire_sink(&mut ed, 1, lane, 2);
    ed.commit().expect("commits");
    let (mut host, mut all) = (Host::new(), Vec::new());
    for n in ragged(16_000, 128) {
        host.roll(&mut ed, &mut exec, n, &mut all);
    }
    let (values, marks) = (&all[0], &all[1]);
    assert!(marks.iter().all(|&m| m != 2.0), "a shaped segment was sent");
    for f in (0..values.len()).step_by(STEP) {
        assert_eq!(marks[f], 1.0, "frame {f}: a grid point");
        let want = curve.value_at(Beat(f as f64 / FPB)).expect("a value");
        assert!(
            (values[f] - want).abs() <= 1e-6,
            "frame {f}: {} vs {want}",
            values[f]
        );
    }
}

/// An LFO's phase survives a depth edit: a 2.5 Hz sine on the play clock,
/// depth 0.25 then 0.4 of a 2000 span, reads `1000 + depth · 2000 ·
/// sin(2π · 2.5 · play / rate)` on every grid frame before the edit and
/// after it (within 1e-6 of depth): the republish changes the depth, not
/// the phase.
///
/// Mutation (run): every point reading the play clock at its block's first
/// frame (`cx.play_at(Offset::ZERO)` in `point`) → the phase stalls inside
/// a block → fails.
#[test]
fn an_lfos_phase_survives_a_depth_edit() {
    let sine = |depth| {
        program(
            None,
            vec![layer(1, seconds(2.5), wave(LfoShape::Sine), amount(depth))],
        )
    };
    let (mut ed, mut exec) = probes(128, &[2], (0.0, 2000.0));
    let lane = native(&ed, 2, sine(0.25), Some(1000.0));
    let controls = wire(&mut ed, 1, lane, 2);
    ed.commit().expect("commits");
    let (mut host, mut all) = (Host::new(), Vec::new());
    for _ in 0..47 {
        host.roll(&mut ed, &mut exec, 100, &mut all);
    }
    let edit = all[0].len();
    controls.publish(sine(0.4));
    for _ in 0..200 {
        host.roll(&mut ed, &mut exec, 100, &mut all);
    }
    let settled = edit.next_multiple_of(STEP);
    for f in (0..all[0].len()).step_by(STEP) {
        if (edit..settled).contains(&f) {
            continue;
        }
        let depth = if f < edit { 0.25 } else { 0.4 };
        let phase = std::f64::consts::TAU * 2.5 * f as f64 / RATE;
        let want = 1000.0 + depth * 2000.0 * phase.sin();
        let got = f64::from(all[0][f]);
        assert!(
            (got - want).abs() <= 1e-6 * depth * 2000.0 + 1e-4,
            "frame {f}: {got} vs {want}"
        );
    }
}

/// A `Linear` triangle is exact at every frame (its pieces are lines
/// between corners), and a `Linear` sine at 10 Hz stays within §5.3's
/// bound, `(2πfh)²/8` of its depth (2.2e-4 at h = 32), at every frame.
///
/// Mutations (run), each fails: the triangle's peaks dropped from its
/// corners → the grid cuts each peak; the grid step doubled (`stride` 64)
/// → the sine's error is four times the bound.
#[test]
fn a_triangle_is_exact_and_a_sine_within_the_bound() {
    let (mut ed, mut exec) = probes(128, &[2, 3], (-10_000.0, 10_000.0));
    let tri = program(
        None,
        vec![layer(
            1,
            seconds(10.0),
            wave(LfoShape::Triangle),
            amount(0.025),
        )],
    );
    let sine = program(
        None,
        vec![layer(1, seconds(10.0), wave(LfoShape::Sine), amount(0.025))],
    );
    // ±500 on a span of 20 000.
    let a = native(&ed, 2, tri, Some(1000.0));
    wire(&mut ed, 10, a, 2);
    let b = native(&ed, 3, sine, Some(1000.0));
    wire(&mut ed, 11, b, 3);
    ed.commit().expect("commits");
    let (mut host, mut all) = (Host::new(), Vec::new());
    for n in ragged(24_000, 128) {
        host.roll(&mut ed, &mut exec, n, &mut all);
    }
    let bound = (std::f64::consts::TAU * 10.0 * STEP as f64 / RATE).powi(2) / 8.0 * 500.0;
    for (f, (&t, &s)) in all[0].iter().zip(&all[1]).enumerate() {
        let c = 10.0 * f as f64 / RATE;
        let tri = 1000.0 + 500.0 * triangle(c);
        let sine = 1000.0 + 500.0 * (std::f64::consts::TAU * c).sin();
        let (t, s) = (f64::from(t), f64::from(s));
        assert!((t - tri).abs() <= 2e-4, "triangle, frame {f}: {t} vs {tri}");
        assert!(
            (s - sine).abs() <= bound + 2e-4,
            "sine, frame {f}: {s} vs {sine}, bound {bound}"
        );
    }
}

/// Capacity (doc 014 §5.3): a 703.125 Hz square (120 edges per 4096
/// frames, between frames and on them) in 4096-frame blocks takes the grid
/// and every edge with nothing thinned or dropped, exact at every frame;
/// a 140.625 Hz square in 16 384-frame blocks (512 grid points and 96
/// edges a block) thins its grid to stay within 256 and counts each block,
/// still exact (a square is flat between its edges).
///
/// Mutations (run), each fails: the capacity check at 200 → the 4096 case
/// thins; no thinning (straight to truncation) → the 16 384 case truncates
/// and misses edges; a square's falling edge left out of its corners
/// (`corners_in`) → that edge lands on the next grid frame.
#[test]
fn the_lane_keeps_every_corner_within_its_capacity() {
    for (max, rate, cycles_per_frame, thinned) in [
        (4096usize, 703.125f32, 15.0 / 1024.0, 0u64),
        (16_384, 140.625, 3.0 / 1024.0, 2),
    ] {
        let (mut ed, mut exec) = probes(max, &[2], (0.0, 2000.0));
        let square = layer(1, seconds(rate), wave(LfoShape::Square), amount(0.25));
        let lane = native(&ed, 2, program(None, vec![square]), Some(1000.0));
        let controls = wire(&mut ed, 1, lane, 2);
        ed.commit().expect("commits");
        let (mut host, mut all) = (Host::new(), Vec::new());
        for _ in 0..2 {
            host.roll(&mut ed, &mut exec, max, &mut all);
        }
        for (f, &v) in all[0].iter().enumerate() {
            let c: f64 = f as f64 * cycles_per_frame;
            let want = if c.fract() < 0.5 { 1500.0 } else { 500.0 };
            assert_eq!(v, want, "{max}-frame blocks, frame {f}");
        }
        assert_eq!(controls.thinned(), thinned, "{max}-frame blocks");
        assert_eq!(controls.truncated(), 0, "{max}-frame blocks");
    }
}

/// An input layer's offset is exactly `depth × span × x` (doc 014 §4.4):
/// knob 10, depth 0.5 of a 0..96 range, a source stepping to 0.5 then
/// 0.25 reads 34.0 then 22.0 from the step's own frame (300, off the
/// grid); a ramp to 1.0 lands 58.0 on its arrival frame. Stopped, so the
/// lane holds between input events.
///
/// Mutations (run), each fails: the input layers left out of the sum →
/// 10; an input event not a point (left out of `run`'s next point) → the
/// step lands on the next grid frame; the arrival not a point (`moving`
/// ignored) → the ramp's end lands late.
#[test]
fn an_input_offset_is_exact_from_its_frame() {
    let (mut ed, mut exec) = probes(128, &[2], (0.0, 96.0));
    let input = InputLayer {
        id: LayerId(1),
        source: SourceId(7),
        amount: amount(0.5),
    };
    let p = ControlProgram::new(None, vec![], vec![input]).expect("valid");
    let lane = native(&ed, 2, p, Some(10.0));
    wire(&mut ed, 1, lane, 2);
    let step = |v| ParamSegment::foreign(7, v, v, Samples(0), SegmentShape::Linear);
    let ramp = ParamSegment::foreign(7, 0.25, 1.0, Samples(200), SegmentShape::Linear);
    let arrive = 600 + ramp.arrives_after().expect("arrives").get();
    ed.insert(
        NodeKey(5),
        "feed",
        ForkByClone(Feed {
            plan: vec![(0, step(0.5)), (300, step(0.25)), (600, ramp)],
        }),
    );
    feed_into(&mut ed, 5, 1);
    ed.commit().expect("commits");
    let stopped = Transport::new(false, TEMPO, Beat(0.0), None);
    let mut all = Vec::new();
    for _ in 0..10 {
        let got = block(&mut ed, &mut exec, &stopped, &TransportChanges::NONE, 128);
        append(&mut all, got);
    }
    let v = &all[0];
    for (f, &got) in v.iter().enumerate() {
        if f < 300 {
            assert_eq!(got, 34.0, "frame {f}");
        } else if f < 600 {
            assert_eq!(got, 22.0, "frame {f}");
        } else if f >= arrive {
            assert_eq!(got, 58.0, "frame {f}");
        } else {
            assert!(got > 22.0 && got < 58.0, "frame {f}: {got}, ramping");
        }
    }
}

/// Connects the `Feed` at `feed` to the lane at `lane`'s event input.
fn feed_into(ed: &mut Editor, feed: u64, lane: u64) {
    ed.spec_mut().connect_events(
        EventIn {
            node: NodeKey(lane),
            port: 0,
        },
        EventEdge::Direct(EventOut {
            node: NodeKey(feed),
            port: 0,
        }),
    );
}

/// **A republish that reads a new source frees a slot for it.** A program
/// reading sources 1..=8 (every input slot, each at 0.0) is republished to
/// read source 9 alone at depth 0.5 of a 0..96 range; source 9 stepping to
/// 0.5 on frame 640 reads 10 + 24 = 34.0 from that frame. (Reported in
/// review: the eight stale slots were never freed, so source 9's
/// events were dropped, uncounted, and the lane held 10.)
///
/// Mutation (run): the slot fallback only a free slot (`Option::is_none`,
/// not a slot whose source the program no longer reads) → 10.0 from frame
/// 640 → fails.
///
/// Changed in doc 014 §12.4 (the mutation record only): a lane now frees
/// every slot its new program does not read when it takes the program, so
/// a free slot is the only fallback. Mutation (run): that freeing dropped
/// (the `generation != self.seen.generation` sweep in `process`) → the
/// eight stale slots stay, source 9 finds none, 10.0 from frame 640 →
/// fails.
#[test]
fn a_republished_program_reads_a_new_source() {
    let (mut ed, mut exec) = probes(128, &[2], (0.0, 96.0));
    let input = |src: u32, depth: f32| InputLayer {
        id: LayerId(u64::from(src)),
        source: SourceId(src),
        amount: amount(depth),
    };
    let eight = (1..=8).map(|k| input(k, 0.1)).collect();
    let p = ControlProgram::new(None, vec![], eight).expect("valid");
    let lane = native(&ed, 2, p, Some(10.0));
    let controls = wire(&mut ed, 1, lane, 2);
    let step =
        |src: u32, v: f32| ParamSegment::foreign(src, v, v, Samples(0), SegmentShape::Linear);
    let mut plan: Vec<(u64, ParamSegment)> = (1..=8).map(|k| (0, step(k, 0.0))).collect();
    plan.push((640, step(9, 0.5)));
    ed.insert(NodeKey(5), "feed", ForkByClone(Feed { plan }));
    feed_into(&mut ed, 5, 1);
    ed.commit().expect("commits");
    let stopped = Transport::new(false, TEMPO, Beat(0.0), None);
    let mut all = Vec::new();
    for k in 0..10 {
        if k == 4 {
            let nine = ControlProgram::new(None, vec![], vec![input(9, 0.5)]).expect("valid");
            controls.publish(nine);
        }
        let got = block(&mut ed, &mut exec, &stopped, &TransportChanges::NONE, 128);
        append(&mut all, got);
    }
    for (f, &got) in all[0].iter().enumerate() {
        let want = if f < 640 { 10.0 } else { 34.0 };
        assert_eq!(got, want, "frame {f}");
    }
}

/// **A reused source id starts from nothing** (doc 014 §7's hazard, closed
/// in §12.4). A lane reads source 3 at depth 0.5 of a 0..96 range on knob
/// 10, and source 3 steps to 0.5 on frame 0: 34.0. A republish drops the
/// source (the lane glides to 10.0 by the next grid point), then another
/// reads source 3 again, as a new source given the old id would be, which
/// has not sent yet: it contributes 0, so the lane reads 10.0, never the
/// removed source's 0.5. When the new source sends 0.25 on frame 1 000,
/// 22.0.
///
/// Mutation (run): the lane keeping unread slots when it takes a program
/// (the sweep in `process` dropped) → after the second republish (from
/// frame 513) the lane glides back to 34.0 → fails. The case where both
/// republishes land before one block (the lane never sees the program
/// between) is closed by the compiler, which never gives an id twice
/// (`tutti-mod`'s `a_signal_source_keeps_its_id`).
#[test]
fn a_reused_source_id_starts_from_nothing() {
    let (mut ed, mut exec) = probes(128, &[2], (0.0, 96.0));
    let reads3 = || {
        let input = InputLayer {
            id: LayerId(1),
            source: SourceId(3),
            amount: amount(0.5),
        };
        ControlProgram::new(None, vec![], vec![input]).expect("valid")
    };
    let lane = native(&ed, 2, reads3(), Some(10.0));
    let controls = wire(&mut ed, 1, lane, 2);
    let step = |v| ParamSegment::foreign(3, v, v, Samples(0), SegmentShape::Linear);
    let plan = vec![(0, step(0.5)), (1_000, step(0.25))];
    ed.insert(NodeKey(5), "feed", ForkByClone(Feed { plan }));
    feed_into(&mut ed, 5, 1);
    ed.commit().expect("commits");
    let stopped = Transport::new(false, TEMPO, Beat(0.0), None);
    let mut all = Vec::new();
    for k in 0..10 {
        if k == 2 {
            controls.publish(ControlProgram::default());
        }
        if k == 4 {
            controls.publish(reads3());
        }
        let got = block(&mut ed, &mut exec, &stopped, &TransportChanges::NONE, 128);
        append(&mut all, got);
    }
    for (f, &got) in all[0].iter().enumerate() {
        let want = match f {
            0..256 => 34.0,
            // The first republish glides to the knob by the next grid
            // point (doc 014 §4.2).
            256..288 => {
                assert!((10.0..=34.0).contains(&got), "frame {f}: {got}, gliding");
                continue;
            }
            288..1_000 => 10.0,
            _ => 22.0,
        };
        assert_eq!(got, want, "frame {f}");
    }
}

/// **Only an input the program reads is a point.** A stopped plugin lane
/// whose program reads source 7 sends one piece at frame 0 and holds; a
/// feed sending source 99 (which it does not read) every 10 frames adds no
/// piece, so an unread source (or MIDI) uses none of the lane's capacity.
///
/// Mutation (run): `run`'s next input point taken from any event
/// (`.find(|_| true)` for the first `input_of`) → a piece on every tenth
/// frame → fails.
#[test]
fn an_unread_source_is_no_point() {
    let (mut ed, mut exec) = probes(128, &[], (0.0, 1.0));
    let input = InputLayer {
        id: LayerId(1),
        source: SourceId(7),
        amount: amount(0.5),
    };
    let p = ControlProgram::new(None, vec![], vec![input]).expect("valid");
    let lane = ControlLaneNode::for_foreign(3, p, Some(0.25), (0.0, 1.0));
    wire_sink(&mut ed, 1, lane, 2);
    let step = |v| ParamSegment::foreign(99, v, v, Samples(0), SegmentShape::Linear);
    let plan = (1..60).map(|k| (k * 10, step(k as f32 / 100.0))).collect();
    ed.insert(NodeKey(5), "feed", ForkByClone(Feed { plan }));
    feed_into(&mut ed, 5, 1);
    ed.commit().expect("commits");
    let stopped = Transport::new(false, TEMPO, Beat(0.0), None);
    let mut all = Vec::new();
    for _ in 0..5 {
        let got = block(&mut ed, &mut exec, &stopped, &TransportChanges::NONE, 128);
        append(&mut all, got);
    }
    let marks: Vec<usize> = (0..all[1].len()).filter(|&f| all[1][f] != 0.0).collect();
    assert_eq!(marks, [0], "one piece, at frame 0");
    assert!(all[0].iter().all(|&v| v == 0.25), "the knob, held");
}

/// **A resync tells a new target a steady lane's value.** A stopped lane
/// with knob 250 sends one held value and nothing more (doc 014 §5.2), so
/// re-wiring it from probe 2 to probe 3 leaves probe 3 at its own base
/// until something moves. `LaneControls::resync` makes the next block send
/// at frame 0: probe 3 hears it on that block's first frame (through the
/// port's connect crossfade) and reads 250.0 once the crossfade ends. This
/// is what the reconcile calls after it re-wires a lane.
///
/// Mutation (run): `process` ignoring the resync flag (`let resync =
/// false`) → probe 3 never reads 250 → fails.
#[test]
fn a_resync_sends_a_steady_lane_to_its_new_target() {
    let (mut ed, mut exec) = probes(64, &[2, 3], (0.0, 1000.0));
    let lane = native(&ed, 2, program(None, vec![]), Some(250.0));
    let controls = wire(&mut ed, 1, lane, 2);
    ed.commit().expect("commits");
    let stopped = Transport::new(false, TEMPO, Beat(0.0), None);
    let run = |ed: &mut Editor, exec: &mut Executor| {
        block(ed, exec, &stopped, &TransportChanges::NONE, 64)
    };
    for _ in 0..3 {
        run(&mut ed, &mut exec);
    }
    let got = run(&mut ed, &mut exec);
    assert!(got[0].iter().all(|&v| v == 250.0), "probe 2 has it");
    let from = EventOut {
        node: NodeKey(1),
        port: 0,
    };
    assert!(ed.spec_mut().disconnect_param(cutoff(2), from));
    ed.spec_mut().connect_param(cutoff(3), from);
    ed.commit().expect("commits");
    for _ in 0..3 {
        let got = run(&mut ed, &mut exec);
        assert!(
            got[1].iter().all(|&v| v != 250.0),
            "steady: probe 3 has not heard it"
        );
    }
    controls.resync();
    // The port's connect crossfade (`PARAM_DECLICK` frames from the commit,
    // 192 frames ago) is still running: the value it fades to is now 250,
    // so frame 0 already moves, and from the crossfade's end the port reads
    // 250 exactly.
    let done = PARAM_DECLICK.get() - 3 * 64;
    let mut after = Vec::new();
    for _ in 0..3 {
        append(&mut after, run(&mut ed, &mut exec));
    }
    assert!(
        after[1][0] > 0.0,
        "frame 0 of the resync's block has heard it"
    );
    for (f, &v) in after[1].iter().enumerate().skip(done) {
        assert_eq!(v, 250.0, "frame {f} after the resync");
    }
}

/// A lane-fed fork reads its value at frame 0 (ported from
/// `a_fork_carries_its_event_sources_ramps`, doc 014 §5.6): the fork's lane
/// is fresh and sends on its first frame, from the program, knob and range
/// at the gather; a later live knob write and republish reach the live
/// lane, not the fork.
///
/// Mutations (run), each fails: `fork_fresh` sharing the live knob cell →
/// the fork reads 700; the fork's lane built on the live lane's `Shared`
/// (`LaneFork` not calling `fork_fresh`) → it reads 50.
#[test]
fn a_lane_fed_fork_reads_its_value_at_frame_0() {
    let (mut ed, mut exec) = probes(64, &[2], (0.0, 1000.0));
    let lane = native(&ed, 2, program(None, vec![]), Some(300.0));
    let controls = wire(&mut ed, 1, lane, 2);
    ed.commit().expect("commits");
    let (mut host, mut live) = (Host::new(), Vec::new());
    for _ in 0..5 {
        host.roll(&mut ed, &mut exec, 64, &mut live);
    }
    assert!(live[0].iter().all(|&v| v == 300.0));

    let (mut fed, mut fexec) = ed
        .fork(
            ForkTarget::Master,
            ForkMode::Offline,
            Prepare::new(SampleRate(RATE), Samples(64)),
        )
        .expect("forks");
    assert!(controls.set_knob(700.0));
    let flat: Arc<dyn Curve> = Arc::new(envelope(&[(0.0, 50.0)]));
    controls.publish(program(Some(flat), vec![]));

    let (mut fhost, mut forked) = (Host::new(), Vec::new());
    for _ in 0..5 {
        fhost.roll(&mut fed, &mut fexec, 64, &mut forked);
    }
    for (f, &v) in forked[0].iter().enumerate() {
        assert_eq!(v, 300.0, "fork, frame {f}");
    }
    for _ in 0..5 {
        host.roll(&mut ed, &mut exec, 64, &mut live);
    }
    assert_eq!(
        *live[0].last().unwrap(),
        50.0,
        "the live lane took the edit"
    );
}

/// `LaneControls`' cells: a plugin lane's knob and range are its own, a
/// native lane's range is its port's (set through the editor), and a lane
/// built without a knob refuses one.
///
/// Mutations (run), each fails: `set_range` accepting a native lane's
/// range (`RangeSource::Port(_) => true`); a native lane snapshotting its
/// port's range into a cell of its own when built → it accepts
/// `set_range`, and misses the editor's edit.
#[test]
fn lane_controls_reach_their_own_cells() {
    let plugin = ControlLaneNode::for_foreign(1, ControlProgram::default(), Some(0.25), (0.0, 1.0));
    let c = plugin.controls();
    assert_eq!(c.knob(), Some(0.25));
    assert!(c.set_knob(0.75));
    assert_eq!(c.knob(), Some(0.75));
    assert!(c.set_range(0.1, 0.9));
    assert_eq!(c.range(), (0.1, 0.9));
    let (mut ed, _exec) = probes(64, &[2], (5.0, 50.0));
    let n = native(&ed, 2, ControlProgram::default(), None);
    let c = n.controls();
    assert!(!c.set_knob(1.0), "no knob");
    assert_eq!(c.knob(), None);
    assert!(!c.set_range(0.0, 1.0), "a native range is its port's");
    assert_eq!(c.range(), (5.0, 50.0));
    ed.set_param_range(cutoff(2), ParamRange::new(6.0, 60.0))
        .expect("a probe");
    assert_eq!(c.range(), (6.0, 60.0), "the port's own cell");
}

/// **A constant layer (a macro) does not move.** A plugin lane whose one
/// layer is `Wave::Const(1.0)` at depth 0.5 of a 0..1 range, on knob 0.25,
/// sends one piece at frame 0 and holds 0.75, rolling (the beat moves, so
/// a beat-clocked wave would walk the grid) and stopped with the layer on
/// a seconds clock (the play clock moves, so a seconds-clocked wave would
/// walk it), and stopped beside a beat-clocked sine (which holds while
/// stopped, at 0 on beat 0), also when that sine's `depth_by` is a
/// constant on a seconds clock: a macro costs the lane one held value, not
/// a piece every `CONTROL_STEP` frames.
///
/// Mutations (run), each fails: the lane's "what moves" counting every
/// layer (`let layers = !self.program.layers().is_empty() || …`) → the
/// rolling lane sends a piece on every grid frame; `has_seconds` counting
/// a constant layer's clock → the stopped lane beside the sine does;
/// `has_seconds` counting a constant `depth_by`'s clock → the stopped lane
/// with the scaled sine does.
#[test]
fn a_constant_layer_is_one_held_value() {
    let konst = |clock| layer(1, clock, Wave::Const(1.0), amount(0.5));
    let sine = layer(2, beats(1.0), wave(LfoShape::Sine), amount(0.25));
    let scaled = Layer {
        depth_by: Some(DepthBy {
            clock: seconds(3.0),
            wave: Wave::Const(1.0),
            amount: DepthBy::UNIT,
        }),
        ..sine.clone()
    };
    let cases = [
        (vec![konst(beats(1.0))], true),
        (vec![konst(seconds(3.0))], false),
        (vec![konst(seconds(3.0)), sine], false),
        (vec![konst(beats(1.0)), scaled], false),
    ];
    for (layers, rolling) in cases {
        let (mut ed, mut exec) = probes(128, &[], (0.0, 1.0));
        let p = program(None, layers);
        let lane = ControlLaneNode::for_foreign(3, p, Some(0.25), (0.0, 1.0));
        wire_sink(&mut ed, 1, lane, 2);
        ed.commit().expect("commits");
        let mut host = Host::new();
        let stopped = Transport::new(false, TEMPO, Beat(0.0), None);
        let mut all = Vec::new();
        for _ in 0..8 {
            if rolling {
                host.roll(&mut ed, &mut exec, 128, &mut all);
            } else {
                let got = block(&mut ed, &mut exec, &stopped, &TransportChanges::NONE, 128);
                append(&mut all, got);
            }
        }
        let marks: Vec<usize> = (0..all[1].len()).filter(|&f| all[1][f] != 0.0).collect();
        assert_eq!(marks, [0], "rolling {rolling}: one piece, at frame 0");
        assert!(
            all[0].iter().all(|&v| v == 0.75),
            "rolling {rolling}: knob + 0.5 × span"
        );
    }
}
