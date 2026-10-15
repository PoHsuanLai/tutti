//! The Bevy-free modulation reconciler (`tutti_nodes::Modulation`, doc 014
//! §8.2, PR 4a): declarations applied to a plain `Editor`, rendered by its
//! executor, against analytic figures and invariants of the render.
//!
//! Each test names the mutations it was seen to fail under. The no-allocation
//! gate across an `apply` that republishes is in `rt_no_alloc_modulation.rs`,
//! beside the allocator it needs.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use tutti_core::{OfflineTimeline, OfflineTimelineConfig};
use tutti_graph::{
    Cx, Editor, Event, EventEdge, EventIn, EventKind, EventOut, Executor, ForkByClone, ForkMode,
    ForkTarget, Io, Node, Offset, ParamFrom, ParamIn, ParamSegment, ParamShaping, Prepare,
    ScheduleError, Shape, Status, Transport, TransportChanges,
};
use tutti_mod::{
    Adsr, Clock, CurveType, Declarations, LfoShape, Polarity, RouteDecl, RouteKey, RouteTarget,
    Source as ModSource, SourceKey, TargetDecl, TargetKey, Wave,
};
use tutti_nodes::automation::{ControlLaneNode, CONTROL_STEP};
use tutti_nodes::testing::Through;
use tutti_nodes::{Feed, ModHost, Modulation, ReconcileError, Target};
use tutti_types::graph::{Edge, InPort, OutPort, Source};
use tutti_types::{
    Amplitude, At, AtomicF32, Beat, Bpm, ChannelLayout, Depth, FrameClock, Hz, NodeKey,
    OfflineTransport, ParamAddr, ParamKey, PhaseIncrement, SampleRate, Samples, Seconds, UnitParam,
};

const RATE: f64 = 48_000.0;
const TEMPO: Bpm = Bpm(120.0);
const STEP: usize = CONTROL_STEP as usize;
const DECLICK: usize = 256;

/// Graph keys of the rig, far from `NodeKey::fresh`'s, which the reconciler
/// mints from 0.
const fn key(n: u64) -> NodeKey {
    NodeKey((1 << 48) + n)
}

// ---------------------------------------------------------------- nodes --

/// Declares one param (`Cutoff` unless told) whose base is `knob`, and
/// writes the value its port reads to its one output. `extra` declares a
/// second param after it, which the probe does not read.
#[derive(Clone)]
struct Probe {
    param: UnitParam,
    extra: Option<UnitParam>,
    knob: Arc<AtomicF32>,
}

impl Probe {
    fn new(knob: f32) -> Self {
        Self {
            param: UnitParam::Cutoff,
            extra: None,
            knob: Arc::new(AtomicF32::new(knob)),
        }
    }
}

impl Node for Probe {
    fn shape(&self) -> Shape {
        let shape = Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO);
        match self.extra {
            None => shape.with_params(&[self.param]),
            Some(extra) => shape.with_params(&[self.param, extra]),
        }
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        match io.param(0).frames() {
            Some(v) => {
                let v: Vec<f32> = v.to_vec();
                io.output(0).copy_from_slice(&v);
            }
            None => {
                let k = self.knob.load(std::sync::atomic::Ordering::Acquire);
                io.output(0).fill(k);
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, port: usize) -> Option<f32> {
        (port == 0).then(|| self.knob.load(std::sync::atomic::Ordering::Acquire))
    }
}

/// Audio on the executor's own clock: `level` on frames `[from, to)`, else
/// 0, on both channels.
#[derive(Clone)]
struct Pulse {
    from: u64,
    to: u64,
    level: f32,
}

impl Node for Pulse {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::STEREO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let base = cx.env.frame.get();
        for c in 0..2 {
            for (i, x) in io.output(c).iter_mut().enumerate() {
                let f = base + i as u64;
                *x = if (self.from..self.to).contains(&f) {
                    self.level
                } else {
                    0.0
                };
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// A transport-locked kick: while rolling, `1 − frac · 10` on the first
/// tenth of every beat; stopped, `bleed`.
#[derive(Clone)]
struct Kick {
    bleed: f32,
}

impl Node for Kick {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::STEREO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        for i in 0..io.frames() {
            let t = cx
                .env
                .transport_at(Offset::new(i, cx.env.block_len).expect("in"));
            let v = if t.playing {
                let frac = t.beat().get().rem_euclid(1.0);
                if frac < 0.1 {
                    (1.0 - frac * 10.0) as f32
                } else {
                    0.0
                }
            } else {
                self.bleed
            };
            io.output(0)[i] = v;
            io.output(1)[i] = v * 0.5;
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// A note-on at executor frame `on`, never released.
#[derive(Clone)]
struct Held {
    on: u64,
}

impl Node for Held {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY).with_events(0, 1)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (base, n) = (cx.env.frame.get(), cx.env.block_len);
        if let Some(o) = self
            .on
            .checked_sub(base)
            .and_then(|o| Offset::new(o as usize, n))
        {
            io.event_out(0)
                .push(Event::midi(o, [0x2090_3C64, 0, 0, 0]))
                .expect("room");
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// Records every segment on its event input, with its executor frame.
#[derive(Clone)]
struct Capture(Arc<Mutex<Vec<(u64, ParamSegment)>>>);

impl Node for Capture {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY).with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
        let mut log = self.0.lock().expect("unpoisoned");
        for e in io.events(0).as_slice() {
            if let EventKind::Segment(s) = e.kind {
                log.push((cx.env.frame_at(e.offset).get(), s));
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

// ---------------------------------------------------------------- host ---

/// The host's side: target node ids (`TargetKey::node`) to graph nodes and
/// their knob cells, plugin-like nodes (event input 0, knob 0.5), and
/// sources' feeds.
#[derive(Default, Clone)]
struct Host {
    targets: BTreeMap<u64, (NodeKey, Arc<AtomicF32>)>,
    foreign: BTreeMap<u64, NodeKey>,
    feeds: BTreeMap<SourceKey, Feed>,
}

impl ModHost for Host {
    fn target(&self, t: TargetKey) -> Option<Target> {
        if let Some(&node) = self.foreign.get(&t.node) {
            return Some(Target::Foreign {
                node,
                port: 0,
                knob: Some(0.5),
            });
        }
        let (node, knob) = self.targets.get(&t.node)?;
        Some(Target::Native {
            node: *node,
            knob: Some(Arc::clone(knob)),
        })
    }
    fn feed(&self, s: SourceKey) -> Option<Feed> {
        self.feeds.get(&s).copied()
    }
}

/// What a host declares, owned.
#[derive(Default, Clone)]
struct Decl {
    sources: Vec<(SourceKey, ModSource)>,
    routes: Vec<(RouteKey, RouteDecl)>,
    targets: Vec<(TargetKey, TargetDecl)>,
}

impl Decl {
    fn get(&self) -> Declarations<'_> {
        Declarations {
            sources: &self.sources,
            routes: &self.routes,
            targets: &self.targets,
        }
    }

    fn source(&mut self, k: u64, s: ModSource) -> SourceKey {
        self.sources.push((SourceKey(k), s));
        SourceKey(k)
    }

    fn route(&mut self, k: u64, source: SourceKey, to: TargetKey, depth: f32) -> RouteKey {
        self.routes.push((
            RouteKey(k),
            RouteDecl {
                source,
                to: RouteTarget::Param(to),
                depth: Depth(depth),
                polarity: Polarity::Bipolar,
                curve: CurveType::Linear,
            },
        ));
        RouteKey(k)
    }

    fn target(&mut self, t: TargetKey, min: f32, max: f32) {
        self.targets.push((
            t,
            TargetDecl {
                min,
                max,
                automation: None,
            },
        ));
    }

    fn drop_route(&mut self, k: RouteKey) {
        self.routes.retain(|(r, _)| *r != k);
    }

    fn drop_source(&mut self, k: SourceKey) {
        self.sources.retain(|(s, _)| *s != k);
    }

    fn set_depth(&mut self, k: RouteKey, depth: f32) {
        for (r, d) in &mut self.routes {
            if *r == k {
                d.depth = Depth(depth);
            }
        }
    }
}

fn cutoff_of(node: u64) -> TargetKey {
    TargetKey {
        node,
        param: ParamAddr::Unit(UnitParam::Cutoff),
    }
}

fn sine(rate: f32) -> ModSource {
    ModSource::Lfo {
        wave: Wave::Shape {
            shape: LfoShape::Sine,
            seed: 0,
        },
        clock: Clock::Seconds {
            rate: Hz(rate),
            phase: PhaseIncrement(0.0),
            anchor: None,
        },
    }
}

fn follower() -> ModSource {
    ModSource::Follower {
        attack: Seconds(0.001),
        release: Seconds(0.05),
    }
}

fn envelope() -> ModSource {
    ModSource::NoteEnvelope {
        adsr: Adsr {
            attack: Seconds(0.002),
            decay: Seconds(0.003),
            sustain: Amplitude(0.4),
            release: Seconds(0.2),
        },
    }
}

// ---------------------------------------------------------------- rigs ---

fn editor(max: usize) -> (Editor, Executor) {
    Editor::new(Prepare::new(SampleRate(RATE), Samples(max)))
}

/// A probe at `key(id)` with knob `knob`, its output appended to the
/// graph's, and the host told it is target node `id`. Committed, so its
/// range cell exists.
fn probe(ed: &mut Editor, host: &mut Host, id: u64, knob: f32) {
    probe_of(ed, host, id, Probe::new(knob));
}

fn probe_of(ed: &mut Editor, host: &mut Host, id: u64, p: Probe) {
    host.targets.insert(id, (key(id), Arc::clone(&p.knob)));
    ed.insert(key(id), "probe", ForkByClone(p));
    ed.spec_mut().topology.outputs.push(Source::Node(OutPort {
        node: key(id),
        port: 0,
    }));
    ed.commit().expect("commits");
}

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

fn append(all: &mut Vec<Vec<f32>>, got: Vec<Vec<f32>>) {
    all.resize(got.len().max(all.len()), Vec::new());
    for (a, g) in all.iter_mut().zip(got) {
        a.extend(g);
    }
}

fn stopped() -> Transport {
    Transport::new(false, TEMPO, Beat(0.0), None)
}

/// `total` stopped frames in blocks of `n`.
fn render(ed: &mut Editor, exec: &mut Executor, n: usize, total: usize) -> Vec<Vec<f32>> {
    let mut all = Vec::new();
    let mut done = 0;
    while done < total {
        let m = n.min(total - done);
        append(
            &mut all,
            block(ed, exec, &stopped(), &TransportChanges::NONE, m),
        );
        done += m;
    }
    all
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

/// `500 + 1000 · depth · sin(2π · rate · play / RATE)`: a bipolar linear
/// sine route on a `0..=1000` target with knob 500, play counted from the
/// executor's first frame (stopped, it runs from 0 there).
fn lfo_value(depth: f64, rate: f64, play: usize) -> f64 {
    500.0 + 1000.0 * depth * (std::f64::consts::TAU * rate * play as f64 / RATE).sin()
}

/// Doc 014 §5.3's bound for a linear sine at `rate` on a 32-frame grid,
/// times the swing, plus `f32` rounding of values near 1000.
fn lfo_tolerance(depth: f64, rate: f64) -> f64 {
    let h = std::f64::consts::TAU * rate * STEP as f64 / RATE;
    h * h / 8.0 * 1000.0 * depth + 1e-3
}

/// The §8.1 rig: an LFO (2 Hz sine, depth 0.3) on probe 1's cutoff
/// (`0..=1000`, knob 500), and a follower of a stereo pulse (0.8 on frames
/// `4800..7200`, attack 1 ms, release 50 ms) on probe 2 (`0..=1`, knob
/// 0.8) at depth −0.6.
struct Rig {
    ed: Editor,
    exec: Executor,
    host: Host,
    decl: Decl,
    m: Modulation,
    lfo: RouteKey,
}

fn rig(max: usize) -> Rig {
    let (mut ed, exec) = editor(max);
    let mut host = Host::default();
    probe(&mut ed, &mut host, 1, 500.0);
    probe(&mut ed, &mut host, 2, 0.8);
    ed.insert(
        key(10),
        "kick",
        ForkByClone(Pulse {
            from: 4_800,
            to: 7_200,
            level: 0.8,
        }),
    );
    ed.commit().expect("commits");
    let mut decl = Decl::default();
    decl.target(cutoff_of(1), 0.0, 1000.0);
    decl.target(cutoff_of(2), 0.0, 1.0);
    let wobble = decl.source(1, sine(2.0));
    let lfo = decl.route(1, wobble, cutoff_of(1), 0.3);
    let kick = decl.source(2, follower());
    host.feeds.insert(kick, Feed::Audio(key(10)));
    decl.route(2, kick, cutoff_of(2), -0.6);
    let mut m = Modulation::new();
    let applied = m.apply(&decl.get(), &mut ed, &host).expect("applies");
    assert!(applied.commit);
    m.commit(&mut ed).expect("commits");
    Rig {
        ed,
        exec,
        host,
        decl,
        m,
        lfo,
    }
}

/// The follower's analytic level at grid point `g`, and its one-step-late
/// linear piece at frame `f` (doc 014 §6): `signal_sources.rs`' figure.
fn ducked(f: usize) -> f64 {
    let pole = |tau: f64| (-1.0 / (tau * RATE)).exp();
    let (ca, cr) = (pole(0.001), pole(0.05));
    let env = |g: usize| -> f64 {
        if g <= 4_800 {
            0.0
        } else if g <= 7_200 {
            0.8 * (1.0 - ca.powi(g as i32 - 4_800))
        } else {
            0.8 * (1.0 - ca.powi(2_400)) * cr.powi(g as i32 - 7_200)
        }
    };
    let g = f / STEP * STEP;
    let x = if g < STEP {
        0.0
    } else {
        let (a, b) = (env(g - STEP), env(g));
        a + (b - a) * (f - g) as f64 / STEP as f64
    };
    0.8 - 0.6 * x
}

// ---------------------------------------------------------------- tests --

/// **Doc 014 §8.1, headless.** An LFO on a native cutoff renders the
/// analytic sine within §5.3's bound (past the connect's declick), and the
/// pad (probe 2) dips with the kick through a follower, analytically.
///
/// Mutations (run), each fails: the lane built with `for_unit(Q, …)` in
/// place of the target's param (the cutoff reads its knob); the route's
/// range not stored (`set_param_range` skipped in `apply`: the port's span
/// is unbounded, so the lane sends the base alone); the follower's feed not
/// wired (`wire_feed` a no-op: the pad never dips).
#[test]
fn the_8_1_example_renders_its_analytic_values() {
    let mut r = rig(128);
    let total = 48_000;
    let all = render(&mut r.ed, &mut r.exec, 128, total);
    let tol = lfo_tolerance(0.3, 2.0);
    for (f, &v) in all[0].iter().enumerate().skip(DECLICK) {
        let want = lfo_value(0.3, 2.0, f);
        assert!(
            (f64::from(v) - want).abs() < tol,
            "frame {f}: {v} vs {want}"
        );
    }
    for (f, &v) in all[1].iter().enumerate() {
        let want = ducked(f);
        assert!(
            (f64::from(v) - want).abs() < 1e-4,
            "frame {f}: {v} vs {want}"
        );
    }
    let low = all[1].iter().copied().fold(f32::INFINITY, f32::min);
    assert!(low < 0.33, "the pad dips: {low}");
}

/// **A depth edit keeps phase (M2) and sends no plan.** The edit is a
/// republish: `apply` asks for no commit, the commit sends nothing (the
/// plan sent last is the same `Arc`), and from one grid step after the
/// block it lands in the cutoff is the same sine at the new depth, its
/// phase unbroken.
///
/// Mutations (run), each fails: the republish dropped (`publish` skipped:
/// the old depth renders on); a program change rebuilding the lane rather
/// than republishing (`Bind::same` false: a commit, and a new plan).
#[test]
fn a_depth_edit_keeps_phase_and_sends_no_plan() {
    let mut r = rig(128);
    let before = render(&mut r.ed, &mut r.exec, 128, 12_800);
    let plan = Arc::clone(r.ed.base().expect("committed"));
    r.decl.set_depth(r.lfo, 0.5);
    let applied =
        r.m.apply(&r.decl.get(), &mut r.ed, &r.host)
            .expect("applies");
    assert!(!applied.commit, "a depth edit needs no commit");
    assert_eq!(applied.republished, vec![cutoff_of(1)]);
    r.m.commit(&mut r.ed).expect("commits");
    assert!(
        Arc::ptr_eq(&plan, r.ed.base().expect("committed")),
        "no plan sent"
    );
    assert_eq!(r.ed.in_flight(), 0, "nothing in flight");
    let after = render(&mut r.ed, &mut r.exec, 128, 24_000);
    let tol = lfo_tolerance(0.5, 2.0);
    for (i, &v) in after[0].iter().enumerate().skip(STEP) {
        let f = 12_800 + i;
        let want = lfo_value(0.5, 2.0, f);
        assert!(
            (f64::from(v) - want).abs() < tol,
            "frame {f}: {v} vs {want}"
        );
    }
    assert!(
        (f64::from(before[0][12_799]) - lfo_value(0.3, 2.0, 12_799)).abs() < tol,
        "the old depth up to the edit"
    );
}

/// **A range edit and a depth edit need no commit; a follower route needs
/// exactly one.** The range edit is a store into the port's range cell
/// (read back through `param_range_cell`), and the render follows it; the
/// follower route (a new source and its edge) is one commit, one plan in
/// flight; applying the same declarations again changes nothing.
///
/// Mutations (run), each fails: the range edit stored nowhere (the cell
/// still reads `0..=1000`); every `apply` asking for a commit; the
/// follower's node and edge committed in two commits (a second `commit` in
/// `apply`'s source loop: two in flight).
#[test]
fn range_and_depth_edits_need_no_commit_and_a_follower_route_one() {
    let (mut ed, mut exec) = editor(128);
    let mut host = Host::default();
    probe(&mut ed, &mut host, 1, 500.0);
    ed.insert(
        key(10),
        "kick",
        ForkByClone(Pulse {
            from: 0,
            to: 1,
            level: 0.0,
        }),
    );
    ed.commit().expect("commits");
    let mut decl = Decl::default();
    decl.target(cutoff_of(1), 0.0, 1000.0);
    let wobble = decl.source(1, sine(2.0));
    let r1 = decl.route(1, wobble, cutoff_of(1), 0.3);
    let mut m = Modulation::new();
    assert!(
        m.apply(&decl.get(), &mut ed, &host)
            .expect("applies")
            .commit
    );
    m.commit(&mut ed).expect("commits");
    render(&mut ed, &mut exec, 128, 1_024);
    let plan = Arc::clone(ed.base().expect("committed"));

    decl.targets[0].1.max = 2000.0;
    decl.set_depth(r1, 0.1);
    let applied = m.apply(&decl.get(), &mut ed, &host).expect("applies");
    assert!(!applied.commit, "range and depth: no commit");
    m.commit(&mut ed).expect("commits");
    assert!(Arc::ptr_eq(&plan, ed.base().expect("committed")));
    let at = ParamIn {
        node: key(1),
        param: UnitParam::Cutoff,
    };
    let cell = ed.param_range_cell(at).expect("a cell").load();
    assert_eq!((cell.min, cell.max), (0.0, 2000.0), "the store landed");
    let got = render(&mut ed, &mut exec, 128, 4_096);
    // Depth 0.1 of a 2000 span: ±200 about 500.
    for (i, &v) in got[0].iter().enumerate().skip(STEP) {
        let p = (1_024 + i) as f64;
        let want = 500.0 + 200.0 * (std::f64::consts::TAU * 2.0 * p / RATE).sin();
        assert!(
            (f64::from(v) - want).abs() < 2e-3,
            "frame {p}: {v} vs {want}"
        );
    }

    let kick = decl.source(2, follower());
    host.feeds.insert(kick, Feed::Audio(key(10)));
    decl.route(2, kick, cutoff_of(1), -0.5);
    ed.collect();
    assert_eq!(ed.in_flight(), 0);
    let applied = m.apply(&decl.get(), &mut ed, &host).expect("applies");
    assert!(applied.commit, "a follower route is a commit");
    m.commit(&mut ed).expect("commits");
    assert_eq!(ed.in_flight(), 1, "exactly one plan sent");
    let again = m.apply(&decl.get(), &mut ed, &host).expect("applies");
    assert_eq!(again, tutti_nodes::Applied::default(), "nothing to do");
}

/// **A removed source leaves no node and no layer (M10).** A follower and
/// an LFO on one cutoff; the follower's source and route go. Its node
/// leaves the spec with its edges, the lane's program reads no input and
/// keeps its LFO layer, and the reconciler holds no node for it.
///
/// Mutations (run), each fails: a gone source's node kept (its
/// `editor.remove` skipped: the node stays in the spec); the republish
/// dropped (the lane still reads the input).
#[test]
fn a_removed_source_leaves_no_node_and_no_layer() {
    let mut r = rig(128);
    let kick = SourceKey(2);
    let node = r.m.source(kick).expect("a follower");
    let mut decl = r.decl.clone();
    decl.drop_route(RouteKey(2));
    decl.drop_source(kick);
    // Probe 2 keeps a lane: an LFO route of its own.
    decl.route(3, SourceKey(1), cutoff_of(2), 0.1);
    render(&mut r.ed, &mut r.exec, 128, 1_024);
    let applied = r.m.apply(&decl.get(), &mut r.ed, &r.host).expect("applies");
    assert!(applied.commit);
    r.m.commit(&mut r.ed).expect("commits");
    assert!(r.m.source(kick).is_none());
    let spec = r.ed.spec();
    assert!(!spec.topology.nodes.contains_key(&node), "the node is gone");
    assert!(
        !spec.topology.edges.keys().any(|at| at.node == node),
        "its input is gone"
    );
    assert!(
        spec.events
            .values()
            .flatten()
            .all(|e| e.from().node != node),
        "its edges are gone"
    );
    let (_, lane) = r.m.lane(cutoff_of(2)).expect("a lane");
    let program = lane.program();
    assert!(program.inputs().is_empty(), "no input layer");
    assert_eq!(program.layers().len(), 1, "the LFO's layer");
    render(&mut r.ed, &mut r.exec, 128, 1_024);
}

/// **A knob written after a route edit is the knob (M9).** The knob is the
/// node's own cell, which the lane reads each block; a route edit
/// republishes the program and writes no knob, so a knob written before or
/// after it is the base the offsets ride on.
///
/// Mutations (run), each fails: `apply` storing the decl's range bottom
/// into the knob on a republish (`set_knob(min)`: the old driver's
/// reseed); the lane given a knob cell of its own (`for_unit` with a copy
/// of the cell: the write never reaches it).
#[test]
fn a_knob_written_after_a_route_edit_is_the_knob() {
    let mut r = rig(128);
    render(&mut r.ed, &mut r.exec, 128, 1_024);
    let knob = Arc::clone(&r.host.targets[&1].1);
    knob.store(300.0, std::sync::atomic::Ordering::Release);
    r.decl.set_depth(r.lfo, 0.2);
    r.m.apply(&r.decl.get(), &mut r.ed, &r.host)
        .expect("applies");
    let a = render(&mut r.ed, &mut r.exec, 128, 2_048);
    knob.store(700.0, std::sync::atomic::Ordering::Release);
    let b = render(&mut r.ed, &mut r.exec, 128, 2_048);
    let tol = lfo_tolerance(0.2, 2.0);
    for (base, from, got) in [(300.0, 1_024, &a[0]), (700.0, 3_072, &b[0])] {
        for (i, &v) in got.iter().enumerate().skip(STEP) {
            let p = (from + i) as f64;
            let want = base + 200.0 * (std::f64::consts::TAU * 2.0 * p / RATE).sin();
            assert!(
                (f64::from(v) - want).abs() < tol,
                "knob {base}, frame {p}: {v} vs {want}"
            );
        }
    }
}

/// The resync rig: a note held from frame 10 into a note envelope (sustain
/// 0.4) routed to probe 1 (`0..=1`, knob 0.2, depth 0.5), and probe 2
/// (`0..=1`, knob 0.1) with a macro of 0 on it, so it has a lane that reads
/// no input. Rendered into the sustain.
fn resync_rig() -> (Rig, SourceKey) {
    let (mut ed, exec) = editor(128);
    let mut host = Host::default();
    probe(&mut ed, &mut host, 1, 0.2);
    probe(&mut ed, &mut host, 2, 0.1);
    ed.insert(key(10), "clip", ForkByClone(Held { on: 10 }));
    ed.commit().expect("commits");
    let mut decl = Decl::default();
    decl.target(cutoff_of(1), 0.0, 1.0);
    decl.target(cutoff_of(2), 0.0, 1.0);
    let env = decl.source(1, envelope());
    host.feeds.insert(
        env,
        Feed::Events(EventOut {
            node: key(10),
            port: 0,
        }),
    );
    decl.route(1, env, cutoff_of(1), 0.5);
    let zero = decl.source(2, ModSource::Macro { value: 0.0 });
    let lfo = decl.route(2, zero, cutoff_of(2), 0.5);
    let mut m = Modulation::new();
    m.apply(&decl.get(), &mut ed, &host).expect("applies");
    m.commit(&mut ed).expect("commits");
    let mut r = Rig {
        ed,
        exec,
        host,
        decl,
        m,
        lfo,
    };
    let got = render(&mut r.ed, &mut r.exec, 128, 4_096);
    assert!(
        (got[0][4_095] - 0.4).abs() < 1e-6,
        "probe 1 in the sustain: {}",
        got[0][4_095]
    );
    assert!((got[1][4_095] - 0.1).abs() < 1e-7, "probe 2 at its knob");
    (r, env)
}

/// **A held note envelope newly routed reads its level at once.** A route
/// from the envelope (held in its sustain) to probe 2's existing lane: a
/// new edge, one commit. From the first grid piece of the block the commit
/// lands in (the lane glides one control step from where it stood, as on
/// any input change), probe 2 reads `0.1 + 0.5 · 0.4` on every frame, not
/// its knob until the next note.
///
/// Mutations (run), each fails: `commit` without `send_resyncs` (probe 2
/// reads 0.1); the note envelope's `Resync` arm dropped (the same);
/// `Applied::resync` never set.
#[test]
fn a_held_note_envelope_newly_routed_reads_its_level_at_once() {
    let (mut r, env) = resync_rig();
    r.decl.route(3, env, cutoff_of(2), 0.5);
    let applied =
        r.m.apply(&r.decl.get(), &mut r.ed, &r.host)
            .expect("applies");
    assert!(applied.commit, "a new edge");
    assert!(applied.resync, "the apply says a resync is owed");
    assert_eq!(r.m.resyncs_owed(), 1);
    r.m.commit(&mut r.ed).expect("commits");
    assert_eq!(r.m.resyncs_owed(), 0, "scheduled");
    let got = render(&mut r.ed, &mut r.exec, 128, 512);
    for (f, &v) in got[1].iter().enumerate().skip(STEP) {
        assert!((v - 0.3).abs() < 1e-6, "frame {f}: {v}");
    }
}

/// **The resync follows the program, not the edge bookkeeping.** The route
/// from the envelope to probe 2 is added and committed; then dropped in an
/// `apply` that is not committed, while blocks run (the lane adopts the
/// program without the envelope and frees its slot, the edge still live);
/// then added back. The spec is back to what was committed, so the commit
/// sends no plan, yet the lane reads the envelope again from 0. The resync
/// is owed because the lane's program newly reads the envelope.
///
/// Why the two triggers agree: a program reads a source exactly when the
/// compile lists its edge, and the reconciler updates its edge set and each
/// lane's published program in the same `apply`, so "newly read" and "a
/// new edge in the reconciler's set" are the same event. The spec's edge
/// is not: here it never left the plan the executor runs.
///
/// Mutation (run): `owe_resyncs` diffing the new program against itself
/// (no source newly read) → probe 2 reads its knob, 0.1 → fails.
#[test]
fn a_route_dropped_and_readded_without_a_commit_resyncs() {
    let (mut r, env) = resync_rig();
    r.decl.route(3, env, cutoff_of(2), 0.5);
    r.m.apply(&r.decl.get(), &mut r.ed, &r.host)
        .expect("applies");
    r.m.commit(&mut r.ed).expect("commits");
    render(&mut r.ed, &mut r.exec, 128, 1_024);
    let plan = Arc::clone(r.ed.base().expect("committed"));

    r.decl.drop_route(RouteKey(3));
    let applied =
        r.m.apply(&r.decl.get(), &mut r.ed, &r.host)
            .expect("applies");
    assert!(applied.commit, "the edge would go");
    let gap = render(&mut r.ed, &mut r.exec, 128, 512);
    assert!(
        (gap[1][511] - 0.1).abs() < 1e-7,
        "the lane dropped the input: {}",
        gap[1][511]
    );

    r.decl.route(3, env, cutoff_of(2), 0.5);
    r.m.apply(&r.decl.get(), &mut r.ed, &r.host)
        .expect("applies");
    assert_eq!(r.m.resyncs_owed(), 1);
    r.m.commit(&mut r.ed).expect("commits");
    assert!(
        Arc::ptr_eq(&plan, r.ed.base().expect("committed")),
        "the spec is what was committed: no plan"
    );
    let got = render(&mut r.ed, &mut r.exec, 128, 256);
    for (f, &v) in got[1].iter().enumerate().skip(STEP) {
        assert!((v - 0.3).abs() < 1e-6, "frame {f}: {v}");
    }
}

/// **A follower has no event input, so a resync cannot reach it**: the
/// editor refuses one scheduled there by port, and the reconciler owes
/// none for a follower (it sends on every grid point while not silent).
///
/// Mutation (run): `owe_resyncs` owing every source kind, and `apply`'s
/// final filter keeping every source with a node → the rig's follower
/// owes one → fails.
#[test]
fn a_follower_takes_no_resync() {
    let mut r = rig(128);
    assert_eq!(r.m.resyncs_owed(), 0, "the rig's follower owes none");
    let node = r.m.source(SourceKey(2)).expect("a follower");
    let to = EventIn { node, port: 0 };
    assert_eq!(
        r.ed.schedule(At::NextBlock, to, EventKind::Resync),
        Err(ScheduleError::NoSuchPort { to })
    );
    r.decl.route(4, SourceKey(2), cutoff_of(1), 0.1);
    r.m.apply(&r.decl.get(), &mut r.ed, &r.host)
        .expect("applies");
    assert_eq!(r.m.resyncs_owed(), 0);
}

/// **A host's own key is never taken.** The host keys its nodes with
/// literals from 0, where `NodeKey::fresh`'s counter also starts (each
/// nextest test is a fresh process): a probe at 0, a pulse at 1. The lane
/// and the follower the reconciler builds land on other keys, the host's
/// nodes keep their kinds, and the pad still dips with the pulse.
///
/// Mutation (run): `unused_key` returning `NodeKey::fresh()` unchecked →
/// the lane replaces the probe at 0 → fails.
#[test]
fn a_host_key_is_never_taken() {
    let (mut ed, mut exec) = editor(128);
    let mut host = Host::default();
    let p = Probe::new(0.8);
    host.targets.insert(1, (NodeKey(0), Arc::clone(&p.knob)));
    ed.insert(NodeKey(0), "probe", ForkByClone(p));
    ed.spec_mut().topology.outputs.push(Source::Node(OutPort {
        node: NodeKey(0),
        port: 0,
    }));
    ed.insert(
        NodeKey(1),
        "pulse",
        ForkByClone(Pulse {
            from: 0,
            to: 2_400,
            level: 0.8,
        }),
    );
    ed.commit().expect("commits");
    render(&mut ed, &mut exec, 128, 128);
    let mut decl = Decl::default();
    decl.target(cutoff_of(1), 0.0, 1.0);
    let pulse = decl.source(1, follower());
    host.feeds.insert(pulse, Feed::Audio(NodeKey(1)));
    decl.route(1, pulse, cutoff_of(1), -0.6);
    let mut m = Modulation::new();
    m.apply(&decl.get(), &mut ed, &host).expect("applies");
    m.commit(&mut ed).expect("commits");
    let lane = m.lane(cutoff_of(1)).expect("a lane").0;
    let source = m.source(pulse).expect("a follower");
    for k in [lane, source] {
        assert!(k != NodeKey(0) && k != NodeKey(1), "{k:?}");
    }
    let nodes = &ed.spec().topology.nodes;
    assert_eq!(nodes[&NodeKey(0)].kind, "probe");
    assert_eq!(nodes[&NodeKey(1)].kind, "pulse");
    let got = render(&mut ed, &mut exec, 128, 2_048);
    assert!(got[0][2_047] < 0.5, "ducked: {}", got[0][2_047]);
}

/// **A foreign target gets a lane of its own normalisation on its event
/// input**: a plugin-like node (a capture) as target node 50, parameter
/// id 7, range `0..=1`, knob 0.5, a macro of 1 at depth 0.25. The lane's
/// segments are addressed `Id(7)` and land on `0.5 + 0.25 · 1`. A range
/// edit to `0..=0.7` is a store into the lane's own range, no commit, and
/// the segments land on `0.5 + 0.25 · 0.7`.
///
/// Mutations (run), each fails: the foreign lane's `connect_events`
/// skipped (no segment reaches the capture); `store_range`'s foreign arm a
/// no-op (the segments stay at 0.75).
#[test]
fn a_foreign_target_takes_a_lane_on_its_event_input() {
    let (mut ed, mut exec) = editor(128);
    let mut host = Host::default();
    let log = Arc::new(Mutex::new(Vec::new()));
    ed.insert(key(50), "plugin", ForkByClone(Capture(Arc::clone(&log))));
    ed.commit().expect("commits");
    host.foreign.insert(50, key(50));
    let param = TargetKey {
        node: 50,
        param: ParamAddr::Id(7),
    };
    let mut decl = Decl::default();
    decl.target(param, 0.0, 1.0);
    let one = decl.source(1, ModSource::Macro { value: 1.0 });
    decl.route(1, one, param, 0.25);
    let mut m = Modulation::new();
    assert!(
        m.apply(&decl.get(), &mut ed, &host)
            .expect("applies")
            .commit
    );
    m.commit(&mut ed).expect("commits");
    render(&mut ed, &mut exec, 128, 512);
    let last = |log: &Mutex<Vec<(u64, ParamSegment)>>| {
        let log = log.lock().expect("unpoisoned");
        let (_, seg) = log.last().copied().expect("a segment");
        assert_eq!(seg.addr(), ParamAddr::Id(7));
        seg.foreign_target(7).expect("its id")
    };
    assert!((last(&log) - 0.75).abs() < 1e-6, "{}", last(&log));

    decl.targets[0].1.max = 0.7;
    let applied = m.apply(&decl.get(), &mut ed, &host).expect("applies");
    assert!(!applied.commit, "a range edit is a store");
    render(&mut ed, &mut exec, 128, 512);
    assert!((last(&log) - 0.675).abs() < 1e-6, "{}", last(&log));
}

/// **A rebind to a node with another param list waits for its commit.**
/// Probe 1 declares `[Cutoff]`; a macro of 1 at depth 0.25 over `0..=1000`
/// reads 250. The host replaces it at its key with a probe declaring
/// `[Cutoff, Q]` (the same knob cell), then applies and commits, as a Bevy
/// frame does (spawn, then commit). Until that commit the port's range
/// cell is the old unit's, which the commit replaces (another list), so
/// the target is unresolved and its lane goes; the host re-applies after
/// the commit, and the lane is rebuilt on the new cell. A range edit to
/// `0..=2000` is then a store the lane reads: 500.
///
/// Mutation (run): `resolve_target` without the committed-generation check
/// → the target resolves before the commit → fails on
/// `unresolved_targets`; with the asserts on the gap removed as well, the
/// lane is rebuilt on the old cell and the range edit reaches nothing (the
/// review's repro): 250 → fails on the last assert.
#[test]
fn a_rebind_to_another_param_list_waits_for_its_commit() {
    let (mut ed, mut exec) = editor(128);
    let mut host = Host::default();
    probe(&mut ed, &mut host, 1, 0.0);
    let mut decl = Decl::default();
    decl.target(cutoff_of(1), 0.0, 1000.0);
    let one = decl.source(1, ModSource::Macro { value: 1.0 });
    decl.route(1, one, cutoff_of(1), 0.25);
    let mut m = Modulation::new();
    m.apply(&decl.get(), &mut ed, &host).expect("applies");
    m.commit(&mut ed).expect("commits");
    let got = render(&mut ed, &mut exec, 128, 1_024);
    assert_eq!(got[0][1_023], 250.0);

    let knob = Arc::clone(&host.targets[&1].1);
    ed.insert(
        key(1),
        "probe",
        ForkByClone(Probe {
            param: UnitParam::Cutoff,
            extra: Some(UnitParam::Q),
            knob,
        }),
    );
    let applied = m.apply(&decl.get(), &mut ed, &host).expect("applies");
    assert_eq!(applied.unresolved_targets, vec![cutoff_of(1)]);
    assert!(m.lane(cutoff_of(1)).is_none(), "the lane went");
    m.commit(&mut ed).expect("commits");
    render(&mut ed, &mut exec, 128, 128);
    let applied = m.apply(&decl.get(), &mut ed, &host).expect("re-applies");
    assert!(applied.unresolved_targets.is_empty());
    assert!(applied.commit, "a lane on the new unit");
    m.commit(&mut ed).expect("commits");
    let got = render(&mut ed, &mut exec, 128, 1_024);
    assert_eq!(got[0][1_023], 250.0);

    decl.targets[0].1.max = 2_000.0;
    let applied = m.apply(&decl.get(), &mut ed, &host).expect("applies");
    assert!(!applied.commit, "a range edit is a store");
    let got = render(&mut ed, &mut exec, 128, 1_024);
    assert_eq!(got[0][1_023], 500.0);
}

/// **A removed feed node leaves its source unresolved, not every apply
/// refused.** The rig's pulse (the follower's feed) is removed, then the
/// LFO's depth is edited: the apply succeeds, the follower is unresolved
/// and its node goes, and the commit is accepted. A note envelope whose
/// MIDI node is removed keeps its node, fed nothing.
///
/// Mutation (run): `resolve_source` without the filter on the feed's node
/// (a missing node back to `WrongFeed`) → both applies refused → fails.
#[test]
fn a_removed_feed_node_leaves_its_source_unresolved() {
    let mut r = rig(128);
    render(&mut r.ed, &mut r.exec, 128, 128);
    r.ed.remove(key(10));
    r.decl.set_depth(r.lfo, 0.2);
    let applied =
        r.m.apply(&r.decl.get(), &mut r.ed, &r.host)
            .expect("applies");
    assert_eq!(applied.unresolved_sources, vec![SourceKey(2)]);
    assert_eq!(applied.republished, vec![cutoff_of(1)]);
    assert!(r.m.source(SourceKey(2)).is_none(), "the follower went");
    r.m.commit(&mut r.ed).expect("commits");
    render(&mut r.ed, &mut r.exec, 128, 512);

    let (mut r, env) = resync_rig();
    r.ed.remove(key(10));
    let applied =
        r.m.apply(&r.decl.get(), &mut r.ed, &r.host)
            .expect("applies");
    assert!(applied.unresolved_sources.is_empty());
    assert!(r.m.source(env).is_some(), "the envelope stays, unfed");
    r.m.commit(&mut r.ed).expect("commits");
}

/// **A port another event source drives is refused before anything
/// changes.** The host drives probe 3's cutoff from an event node of its
/// own; a route to that cutoff is `PortTaken`, with the editor and the
/// reconciler as before, and the editor still commits.
///
/// Mutation (run): the `PortTaken` check removed → the apply succeeds,
/// wiring a second event source into the port → fails.
#[test]
fn a_port_with_another_event_source_is_refused() {
    let mut r = rig(128);
    render(&mut r.ed, &mut r.exec, 128, 128);
    probe(&mut r.ed, &mut r.host, 3, 0.5);
    r.ed.insert(key(30), "clip", ForkByClone(Held { on: 10 }));
    r.ed.spec_mut().connect_param(
        ParamIn {
            node: key(3),
            param: UnitParam::Cutoff,
        },
        ParamFrom::Events(EventOut {
            node: key(30),
            port: 0,
        }),
        ParamShaping::Identity,
    );
    r.ed.commit().expect("commits");
    render(&mut r.ed, &mut r.exec, 128, 128);
    let targets = [cutoff_of(1), cutoff_of(2), cutoff_of(3)];
    let sources = [SourceKey(1), SourceKey(2)];
    let before = state(&r.ed, &r.m, &targets, &sources);
    let mut decl = r.decl.clone();
    decl.target(cutoff_of(3), 0.0, 1.0);
    decl.route(9, SourceKey(1), cutoff_of(3), 0.1);
    assert_eq!(
        r.m.apply(&decl.get(), &mut r.ed, &r.host),
        Err(ReconcileError::PortTaken(cutoff_of(3)))
    );
    assert_eq!(state(&r.ed, &r.m, &targets, &sources), before);
    r.ed.commit().expect("the editor still commits");
}

/// **A key the host takes from a built node is followed.** The host
/// inserts nodes of its own at the LFO lane's key and at the follower's:
/// the next apply undoes the wiring around both, leaves the host's nodes
/// and builds a new lane and follower at fresh keys. The render is the
/// §8.1 figures again (the LFO past the new lane's declick).
///
/// Mutation (run): `taken` always false → the apply sees nothing to do
/// (no commit, the records still name the host's nodes) → fails.
#[test]
fn a_key_the_host_takes_is_followed() {
    let mut r = rig(128);
    let mut all = render(&mut r.ed, &mut r.exec, 128, 128);
    let lane = r.m.lane(cutoff_of(1)).expect("a lane").0;
    let follower = r.m.source(SourceKey(2)).expect("a follower");
    r.ed.insert(lane, "probe", ForkByClone(Probe::new(1.0)));
    r.ed.insert(
        follower,
        "pulse",
        ForkByClone(Pulse {
            from: 0,
            to: 0,
            level: 0.0,
        }),
    );
    let applied =
        r.m.apply(&r.decl.get(), &mut r.ed, &r.host)
            .expect("applies");
    assert!(applied.commit);
    let (new_lane, new_follower) = (
        r.m.lane(cutoff_of(1)).expect("a lane").0,
        r.m.source(SourceKey(2)).expect("a follower"),
    );
    assert!(new_lane != lane && new_follower != follower);
    let nodes = &r.ed.spec().topology.nodes;
    assert_eq!(nodes[&lane].kind, "probe");
    assert_eq!(nodes[&follower].kind, "pulse");
    r.m.commit(&mut r.ed).expect("commits");
    append(&mut all, render(&mut r.ed, &mut r.exec, 128, 12_000 - 128));
    let tol = lfo_tolerance(0.3, 2.0);
    for (f, &v) in all[0].iter().enumerate().skip(128 + DECLICK + STEP) {
        let want = lfo_value(0.3, 2.0, f);
        assert!(
            (f64::from(v) - want).abs() < tol,
            "frame {f}: {v} vs {want}"
        );
    }
    for (f, &v) in all[1].iter().enumerate() {
        let want = ducked(f);
        assert!(
            (f64::from(v) - want).abs() < 1e-4,
            "frame {f}: {v} vs {want}"
        );
    }
}

/// Snapshot of everything a refused `apply` must leave alone.
fn state(ed: &Editor, m: &Modulation, targets: &[TargetKey], sources: &[SourceKey]) -> String {
    format!(
        "{:?}\n{:?}\n{:?}\n{:?}\n{}",
        ed.spec(),
        m.compiled(),
        targets
            .iter()
            .map(|t| m.lane(*t).map(|l| l.0))
            .collect::<Vec<_>>(),
        sources.iter().map(|s| m.source(*s)).collect::<Vec<_>>(),
        m.resyncs_owed(),
    )
}

/// **A follower downstream of its own target is refused by route**, before
/// anything changes: directly (the follower reads probe 1, whose cutoff it
/// modulates) and through a bus (probe 1 → a bus → the follower). The
/// error names the route, and the editor's spec, the compile, the lanes
/// and the source nodes are exactly as before. A follower of something
/// upstream (the kick) on the same target is fine.
///
/// Mutations (run), each fails: `check_cycles` not called (both apply,
/// and the graph's commit would refuse later); the walk not following
/// audio edges (the bus case passes); the walk not seeded with the feeds
/// (both pass).
#[test]
fn a_follower_downstream_of_its_target_is_refused_by_route() {
    let mut r = rig(128);
    render(&mut r.ed, &mut r.exec, 128, 128);
    r.ed.insert(
        key(20),
        "bus",
        ForkByClone(Through::new(ChannelLayout::MONO)),
    );
    r.ed.spec_mut().topology.edges.insert(
        InPort {
            node: key(20),
            port: 0,
        },
        Edge::Direct(Source::Node(OutPort {
            node: key(1),
            port: 0,
        })),
    );
    r.ed.commit().expect("commits");
    render(&mut r.ed, &mut r.exec, 128, 128);
    let targets = [cutoff_of(1), cutoff_of(2)];
    let sources = [SourceKey(1), SourceKey(2), SourceKey(3)];
    for feed in [key(1), key(20)] {
        let before = state(&r.ed, &r.m, &targets, &sources);
        let mut decl = r.decl.clone();
        let mut host = r.host.clone();
        let loop_ = decl.source(3, follower());
        host.feeds.insert(loop_, Feed::Audio(feed));
        decl.route(7, loop_, cutoff_of(1), 0.2);
        assert_eq!(
            r.m.apply(&decl.get(), &mut r.ed, &host),
            Err(ReconcileError::Cycle { route: RouteKey(7) }),
            "fed by {feed:?}"
        );
        assert_eq!(state(&r.ed, &r.m, &targets, &sources), before);
    }
    let mut decl = r.decl.clone();
    decl.route(7, SourceKey(2), cutoff_of(1), 0.2);
    r.m.apply(&decl.get(), &mut r.ed, &r.host)
        .expect("the kick is upstream");
    r.m.commit(&mut r.ed).expect("the graph agrees");
}

fn offline() -> OfflineTransport {
    OfflineTransport::new(Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        sample_rate: SampleRate(RATE),
        ..Default::default()
    })))
}

/// The host's playhead: a frame clock from beat 0.
struct Playhead(FrameClock);

impl Playhead {
    fn new() -> Self {
        Self(FrameClock::new(Beat(0.0), TEMPO, SampleRate(RATE)))
    }
    fn roll(&mut self, ed: &mut Editor, exec: &mut Executor, n: usize, all: &mut Vec<Vec<f32>>) {
        let t = Transport::counted(true, TEMPO, self.0.origin(), None);
        append(all, block(ed, exec, &t, &TransportChanges::NONE, n));
        self.0.advance(Samples(n), None);
    }
}

/// **Export == live on a native param, through a fork of the editor.** An
/// LFO on a seconds clock on probe 1 and a follower of a transport-locked
/// kick on probe 2, built by `apply`; live runs stopped (the kick bleeding
/// into the follower), then starts inside a block and renders ragged
/// blocks; the export is an offline fork taken before the start, in
/// 1024-frame blocks. Bit for bit from the play start.
///
/// The reference CLAP plugin's gain at a pinned chunk size is PR 4b's:
/// a plugin is `tutti-plugin`'s, which `tutti-nodes` cannot name.
///
/// Mutations (run), each fails: the lane's fork dropping the knob it
/// gathers (`fork_fresh`'s knob `None`: the export's cutoff has no base);
/// the follower inserted `Unforkable` (the fork refuses).
#[test]
fn an_export_is_the_live_render_on_a_native_param() {
    let (mut ed, mut exec) = editor(128);
    let mut host = Host::default();
    probe(&mut ed, &mut host, 1, 500.0);
    probe(&mut ed, &mut host, 2, 0.8);
    ed.insert(key(10), "kick", ForkByClone(Kick { bleed: 0.8 }));
    ed.commit().expect("commits");
    let mut decl = Decl::default();
    decl.target(cutoff_of(1), 0.0, 1000.0);
    decl.target(cutoff_of(2), 0.0, 1.0);
    let wobble = decl.source(1, sine(3.0));
    decl.route(1, wobble, cutoff_of(1), 0.3);
    let kick = decl.source(2, follower());
    host.feeds.insert(kick, Feed::Audio(key(10)));
    decl.route(2, kick, cutoff_of(2), -0.6);
    let mut m = Modulation::new();
    m.apply(&decl.get(), &mut ed, &host).expect("applies");
    m.commit(&mut ed).expect("commits");
    render(&mut ed, &mut exec, 128, 384);
    host.targets[&1]
        .1
        .store(450.0, std::sync::atomic::Ordering::Release);

    let offline = offline();
    let (mut fed, mut fexec) = ed
        .fork(
            ForkTarget::Master,
            ForkMode::Offline(&offline),
            Prepare::new(SampleRate(RATE), Samples(1024)),
        )
        .expect("forks");
    let total = 48_000;
    let mut head = Playhead::new();
    let mut ch = TransportChanges::NONE;
    let start = Transport::counted(true, TEMPO, head.0.origin(), None);
    ch.push(Offset::new(37, Samples(128)).expect("in"), start)
        .expect("room");
    let mut live = block(&mut ed, &mut exec, &stopped(), &ch, 128);
    for c in &mut live {
        c.drain(..37);
    }
    head.0.advance(Samples(128 - 37), None);
    for n in ragged(total - (128 - 37), 128) {
        head.roll(&mut ed, &mut exec, n, &mut live);
    }
    let (mut head, mut export) = (Playhead::new(), Vec::new());
    while export.first().map_or(0, Vec::len) < total {
        head.roll(&mut fed, &mut fexec, 1024, &mut export);
    }
    for (c, (l, e)) in live.iter().zip(&export).enumerate() {
        assert!(l.iter().any(|&v| v != l[0]), "channel {c} moves");
        for f in 0..total {
            assert_eq!(
                l[f].to_bits(),
                e[f].to_bits(),
                "channel {c}, frame {f}: live {} vs export {}",
                l[f],
                e[f]
            );
        }
    }
}

/// **`for_param::<Hz>` and `for_unit(Cutoff)` send the same segments**, bit
/// for bit: the typed constructor is a spelling of the erased one. And a
/// lane on a param with no typed key (`Pan`) reaches its port.
///
/// Mutations (run), each fails: `ParamSegment::unit` marking its segment
/// foreign (the pan port reads its knob); `for_param` passing another
/// param than its key's (`for_unit(UnitParam::Q, …)`: the addresses
/// differ).
#[test]
fn for_param_and_for_unit_send_the_same_segments() {
    let (mut ed, mut exec) = editor(128);
    let program = || {
        let mut decl = Decl::default();
        decl.target(cutoff_of(1), 0.0, 1000.0);
        let s = decl.source(1, sine(7.0));
        decl.route(1, s, cutoff_of(1), 0.4);
        let c = tutti_mod::compile(
            &decl.get(),
            None,
            tutti_graph::PlayReading::default(),
            SampleRate(RATE),
        )
        .expect("compiles");
        c.programs[&cutoff_of(1)].clone()
    };
    let logs: Vec<Arc<Mutex<Vec<(u64, ParamSegment)>>>> =
        (0..2).map(|_| Arc::new(Mutex::new(Vec::new()))).collect();
    let knob = || Some(Arc::new(AtomicF32::new(500.0)));
    // No range cell: the lanes send the knob plus the offset unscaled,
    // which is all a segment's bits need.
    let lanes: [ControlLaneNode; 2] = [
        ControlLaneNode::for_param(ParamKey::<Hz>::CUTOFF, program(), knob(), None),
        ControlLaneNode::for_unit(UnitParam::Cutoff, program(), knob(), None),
    ];
    for (i, lane) in lanes.into_iter().enumerate() {
        let (l, c) = (key(30 + i as u64), key(40 + i as u64));
        ed.insert(l, "lane", lane);
        ed.insert(c, "capture", ForkByClone(Capture(Arc::clone(&logs[i]))));
        ed.spec_mut().connect_events(
            EventIn { node: c, port: 0 },
            EventEdge::Direct(EventOut { node: l, port: 0 }),
        );
    }
    ed.commit().expect("commits");
    render(&mut ed, &mut exec, 128, 4_096);
    let bits = |log: &Arc<Mutex<Vec<(u64, ParamSegment)>>>| -> Vec<String> {
        log.lock()
            .expect("unpoisoned")
            .iter()
            .map(|(f, s)| format!("{f} {:?} {s:?}", s.addr()))
            .collect()
    };
    let (a, b) = (bits(&logs[0]), bits(&logs[1]));
    assert!(a.len() > 100, "a grid's worth: {}", a.len());
    assert_eq!(a, b);

    // A param with no `ParamKey`: the lane reaches its port all the same.
    let mut host = Host::default();
    probe_of(
        &mut ed,
        &mut host,
        5,
        Probe {
            param: UnitParam::Pan,
            extra: None,
            knob: Arc::new(AtomicF32::new(0.0)),
        },
    );
    let mut decl = Decl::default();
    let pan = TargetKey {
        node: 5,
        param: ParamAddr::Unit(UnitParam::Pan),
    };
    decl.target(pan, -1.0, 1.0);
    let s = decl.source(1, ModSource::Macro { value: 1.0 });
    decl.route(1, s, pan, 0.25);
    let mut m = Modulation::new();
    m.apply(&decl.get(), &mut ed, &host).expect("applies");
    m.commit(&mut ed).expect("commits");
    let got = render(&mut ed, &mut exec, 128, 512);
    // 0 + 0.25 · 1 · 2 (the span), past the connect's declick.
    assert_eq!(got.last().expect("the pan probe")[511], 0.5);
}
