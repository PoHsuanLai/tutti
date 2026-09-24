//! Event delivery properties the differential suite cannot see: rules where
//! the reference and the executor could share one wrong decision.
//!
//! - **Conservation** (review S1): every event emitted while an edge exists
//!   is delivered exactly once to that edge's sink, across recompiles that
//!   retune, remove, rewire and re-add delays and feedback edges, regenerate
//!   units, and change the block size — checked on *both* interpreters
//!   against a ledger kept by the emitters themselves.
//! - **Wide fan-in** (B1): 65 and 200 sources into one event port compile to
//!   a merge tree and deliver in `(offset, source order)`.
//! - **Feedback delays by its declared delay** at any `MaxBlock`, audio and events, under
//!   block sizes that change and are ragged.
//! - **Scheduled commands are conserved too**: each lands exactly once, on
//!   its frame (or, when already past, at the start of the next block),
//!   through the same recompiles.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use common::prepare;
use proptest::prelude::*;
use tutti_graph::{
    compile, Cx, Editor, Event, EventEdge, EventIn, EventKind, EventOut, Executor, GraphSpec, Io,
    Node, Prepare, Reference, Shape, Shapes, Status, Transport, Ump,
};
use tutti_types::graph::{Edge, FeedbackFrom, InPort, NodeSpec, OutPort, Source};
use tutti_types::{At, ChannelLayout, Frame, Latency, NodeKey, Samples, Tail, Topology};

type Ledger = Arc<Mutex<Vec<(u32, u64)>>>;
type Inbox = Arc<Mutex<Vec<(u16, u32, u64, u64)>>>;

/// Emits `[id, frame]` every `period` frames and writes each emission to its
/// ledger.
struct Emitter {
    id: u32,
    period: u64,
    ledger: Ledger,
    /// Set for the drain: emit nothing more, so everything in flight lands.
    stop: Arc<AtomicBool>,
}

impl Node for Emitter {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY).with_events(0, 1)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        if self.stop.load(Ordering::Relaxed) {
            return Status::Silent;
        }
        for at in cx.env.offsets() {
            let f = cx.env.frame_at(at).get();
            if f % self.period == u64::from(self.id) % self.period {
                io.event_out(0)
                    .push(Event::midi(at, [self.id, f as u32, (f >> 32) as u32, 0]))
                    .expect("capacity is sized for the test's rate");
                self.ledger.lock().unwrap().push((self.id, f));
            }
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// Logs `(port, emitter id, emit frame, delivery frame)` for every event on
/// each of its event inputs. One audio input, only so a latent node can raise
/// its arrival and so put PDC delays on its event inputs.
struct Recorder {
    ports: u16,
    inbox: Inbox,
}

impl Node for Recorder {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::MONO, ChannelLayout::EMPTY).with_events(self.ports, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
        let mut inbox = self.inbox.lock().unwrap();
        for p in 0..io.event_input_count() {
            for e in io.events(p) {
                let EventKind::Midi(Ump(w)) = e.kind else {
                    panic!("only MIDI is sent")
                };
                let emitted = u64::from(w[1]) | (u64::from(w[2]) << 32);
                inbox.push((p as u16, w[0], emitted, cx.env.frame_at(e.offset).get()));
            }
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// A pure audio delay declaring `latency` frames (to make PDC happen).
struct Lag {
    latency: usize,
}

impl Node for Lag {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO)
            .with_latency(Latency::new(Samples(self.latency)))
            .with_tail(Tail::Unknown)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(0.0);
        Status::Silent
    }
    fn reset(&mut self) {}
}

const REC: NodeKey = NodeKey(1000);
const LAG: NodeKey = NodeKey(2000);
const REC_PORTS: u16 = 3;
const MAX: usize = 32;
const CAP: usize = 256;

/// One phase of the conservation scenario.
#[derive(Clone, Debug)]
struct Phase {
    lag: usize,
    lag_gen: u32,
    emitter_gen: Vec<u32>,
    /// Per recorder port: (emitter index, feedback?).
    edges: Vec<Vec<(usize, bool)>>,
    blocks: Vec<usize>,
    /// Commands scheduled as the phase starts: (recorder port, frame
    /// relative to the phase's first frame — negative is already late).
    commands: Vec<(u16, i64)>,
}

/// Recorder tags at or above this are scheduled commands, not emissions.
const SCHEDULED: u32 = 10_000;

fn spec_of(ph: &Phase, emitters: &[(u32, u64)]) -> (GraphSpec, Shapes) {
    let mut t = Topology::default();
    let mut shapes = Shapes::new();
    let mut add = |t: &mut Topology, k: NodeKey, s: Shape| {
        t.nodes.insert(
            k,
            NodeSpec::new("n", s.audio_in, s.audio_out)
                .with_latency(s.latency.samples())
                .with_tail(s.tail),
        );
        shapes.insert(k, s);
    };
    for (i, _) in emitters.iter().enumerate() {
        add(
            &mut t,
            NodeKey(i as u64),
            Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY).with_events(0, 1),
        );
    }
    add(&mut t, LAG, Lag { latency: ph.lag }.shape());
    add(
        &mut t,
        REC,
        Shape::audio(ChannelLayout::MONO, ChannelLayout::EMPTY).with_events(REC_PORTS, 0),
    );
    t.edges.insert(
        InPort { node: REC, port: 0 },
        Edge::Direct(Source::Node(OutPort { node: LAG, port: 0 })),
    );
    let mut g = GraphSpec::new(t);
    for (p, srcs) in ph.edges.iter().enumerate() {
        for &(e, fb) in srcs {
            let from = EventOut {
                node: NodeKey(e as u64),
                port: 0,
            };
            let at = EventIn {
                node: REC,
                port: p as u16,
            };
            g.connect_events(
                at,
                if fb {
                    EventEdge::feedback(from, Samples(MAX + 5))
                } else {
                    EventEdge::Direct(from)
                },
            );
        }
    }
    for (i, &gen) in ph.emitter_gen.iter().enumerate() {
        g.generations.insert(NodeKey(i as u64), gen);
    }
    g.generations.insert(LAG, ph.lag_gen);
    (g, shapes)
}

/// Fresh units for every node of a phase, logging into `ledger` / `inbox`.
fn units_of(
    ph: &Phase,
    emitters: &[(u32, u64)],
    ledger: &Ledger,
    inbox: &Inbox,
    stop: &Arc<AtomicBool>,
) -> BTreeMap<NodeKey, Box<dyn Node>> {
    let mut u: BTreeMap<NodeKey, Box<dyn Node>> = BTreeMap::new();
    for (i, &(id, period)) in emitters.iter().enumerate() {
        u.insert(
            NodeKey(i as u64),
            Box::new(Emitter {
                id,
                period,
                ledger: Arc::clone(ledger),
                stop: Arc::clone(stop),
            }),
        );
    }
    u.insert(LAG, Box::new(Lag { latency: ph.lag }));
    u.insert(
        REC,
        Box::new(Recorder {
            ports: REC_PORTS,
            inbox: Arc::clone(inbox),
        }),
    );
    u
}

/// Run the phases through the executor (`true`) or the reference, then drain.
/// Returns (emissions tagged with phase, deliveries).
/// A scheduled command's tag and the frame it must land on.
type Landing = (u16, u32, u64);

fn run(
    phases: &[Phase],
    emitters: &[(u32, u64)],
    executor: bool,
) -> (Vec<(usize, u32, u64)>, Vec<(u16, u32, u64, u64)>) {
    let (emitted, got, _) = run_scheduled(phases, emitters, executor);
    (emitted, got)
}

/// `run`, also returning where every scheduled command must land:
/// `(port, tag, frame)`, the frame being its target or — when that was
/// already past as it was scheduled — the first frame of the next block.
fn run_scheduled(
    phases: &[Phase],
    emitters: &[(u32, u64)],
    executor: bool,
) -> (
    Vec<(usize, u32, u64)>,
    Vec<(u16, u32, u64, u64)>,
    Vec<Landing>,
) {
    let mut landings: Vec<Landing> = Vec::new();
    let mut now = 0u64;
    let ledger: Ledger = Arc::default();
    let inbox: Inbox = Arc::default();
    let stop = Arc::new(AtomicBool::new(false));
    let prep = prepare(MAX);
    let (mut ed, mut exec): (Editor, Executor) = Editor::with_event_capacity(prep, CAP);
    let mut reference = Reference::new(prep);
    let mut phase_of: Vec<(usize, u32, u64)> = Vec::new();
    let transport = Transport::default();
    let mut all = phases.to_vec();
    all.push(ph_drain(phases));
    for (k, ph) in all.iter().enumerate() {
        if k == phases.len() {
            stop.store(true, Ordering::Relaxed);
        }
        let (g, shapes) = spec_of(ph, emitters);
        let valid = g.validate().expect("valid");
        if executor {
            let (plan, delta) =
                compile(&valid, &shapes, &prep, ed.base().map(|p| &**p)).expect("compiles");
            let placed: BTreeSet<NodeKey> = delta
                .insert
                .iter()
                .map(|p| p.key)
                .chain(delta.replace.iter().map(|(_, n)| n.key))
                .collect();
            let mut units = units_of(ph, emitters, &ledger, &inbox, &stop);
            units.retain(|k, _| placed.contains(k));
            ed.package(plan, delta, units).expect("room in the queue");
            exec.apply_pending();
            ed.collect();
        } else {
            reference.set_graph(&valid, units_of(ph, emitters, &ledger, &inbox, &stop));
        }
        for &(port, rel) in &ph.commands {
            let tag = SCHEDULED + landings.len() as u32;
            let target = now.saturating_add_signed(rel);
            let kind = EventKind::Midi(Ump([tag, target as u32, (target >> 32) as u32, 0]));
            let at = EventIn { node: REC, port };
            let when = At::Frame(Frame(target));
            if executor {
                ed.schedule(when, at, kind)
                    .expect("room for the test's commands");
            } else {
                reference.schedule(when, at, kind);
            }
            landings.push((port, tag, target.max(now)));
        }
        let before = ledger.lock().unwrap().len();
        for &n in &ph.blocks {
            now += n as u64;
            if executor {
                exec.process(n, &transport, &[], &mut []);
            } else {
                reference.process(n, &transport, &[], &mut []);
            }
        }
        for &(id, f) in &ledger.lock().unwrap()[before..] {
            phase_of.push((k, id, f));
        }
    }
    if executor {
        assert_eq!(exec.dropped_events(), 0, "nothing was dropped");
        assert_eq!(exec.unrouted_commands(), 0, "the recorder never went away");
        assert_eq!(ed.commands_outstanding(), 0, "every command landed");
    }
    let got = inbox.lock().unwrap().clone();
    (phase_of, got, landings)
}

/// The drain phase: the last phase's wiring (so nothing is recompiled away),
/// emitters stopped, and blocks enough for every delay to empty.
fn ph_drain(phases: &[Phase]) -> Phase {
    let last = phases.last().expect("at least one phase").clone();
    Phase {
        blocks: vec![MAX; 8],
        commands: Vec::new(),
        ..last
    }
}

/// What each recorder port should have received: every emission made during
/// a phase in which that emitter fed that port (directly or by feedback).
fn expected(
    phases: &[Phase],
    emitted: &[(usize, u32, u64)],
    emitters: &[(u32, u64)],
) -> BTreeMap<u16, Vec<(u32, u64)>> {
    let mut all = phases.to_vec();
    all.push(ph_drain(phases));
    let mut want: BTreeMap<u16, Vec<(u32, u64)>> = BTreeMap::new();
    for &(k, id, f) in emitted {
        let e = emitters.iter().position(|&(i, _)| i == id).unwrap();
        for (p, srcs) in all[k].edges.iter().enumerate() {
            if srcs.iter().any(|&(s, _)| s == e) {
                want.entry(p as u16).or_default().push((id, f));
            }
        }
    }
    for v in want.values_mut() {
        v.sort();
    }
    want
}

fn delivered(got: &[(u16, u32, u64, u64)]) -> BTreeMap<u16, Vec<(u32, u64)>> {
    let mut m: BTreeMap<u16, Vec<(u32, u64)>> = BTreeMap::new();
    for &(p, id, f, _) in got.iter().filter(|e| e.1 < SCHEDULED) {
        m.entry(p).or_default().push((id, f));
    }
    for v in m.values_mut() {
        v.sort();
    }
    m
}

fn arb_phase(emitters: usize) -> impl Strategy<Value = Phase> {
    (
        prop_oneof![Just(0usize), 1usize..40],
        0u32..2,
        proptest::collection::vec(0u32..2, emitters),
        proptest::collection::vec(
            proptest::collection::btree_map(0..emitters, any::<bool>(), 0..=emitters),
            REC_PORTS as usize,
        ),
        proptest::collection::vec(1usize..=MAX, 1..12),
        proptest::collection::vec((0..REC_PORTS, -40i64..200), 0..8),
    )
        .prop_map(
            |(lag, lag_gen, emitter_gen, edges, blocks, commands)| Phase {
                lag,
                lag_gen,
                emitter_gen,
                edges: edges.into_iter().map(|m| m.into_iter().collect()).collect(),
                blocks,
                commands,
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    /// S1: every event emitted while an edge exists is delivered exactly
    /// once to that edge's sink — no loss, no duplicate — through PDC delays
    /// that retune or vanish (lag at 0), rewiring, direct↔feedback switches,
    /// regenerated emitters and ragged block sizes. Checked on both
    /// interpreters, against the emitters' own ledger.
    ///
    /// Mutation: in `Executor::rebuild`, skip the flush of disappeared event
    /// delays (`gone` loop) → events pending in a vanished delay are lost →
    /// fails. The same in `Reference::set_graph` → fails on the reference.
    /// Mutation: drop the `inject` prepend in the reference's event gather →
    /// fails. Mutation: in `EventFifo::pop_due`, drop events when `out` is
    /// full instead of leaving them queued (with a small CAP) → fails.
    ///
    /// And for scheduled commands — each lands exactly once, on its port, on
    /// its frame or (already past) at the next block's start. Mutation: in
    /// `CommandRx::gather`, keep a delivered command pending → it lands
    /// again every block → fails. Mutation: in `CommandRx::overlay`, merge
    /// only the scheduled events, dropping the port's own → the emitters'
    /// events vanish → fails. Mutation: drop the reference's scheduled
    /// events from its gather → fails on the reference. Mutation: land a
    /// late command at offset 1 instead of 0 in the reference → fails.
    #[test]
    fn every_event_is_delivered_exactly_once(
        phases in proptest::collection::vec(arb_phase(4), 1..4),
    ) {
        let emitters: Vec<(u32, u64)> = vec![(1, 3), (2, 5), (3, 7), (4, 4)];
        for executor in [true, false] {
            let (emitted, got, landings) = run_scheduled(&phases, &emitters, executor);
            let want = expected(&phases, &emitted, &emitters);
            let have = delivered(&got);
            let side = if executor { "executor" } else { "reference" };
            for p in 0..REC_PORTS {
                prop_assert_eq!(
                    want.get(&p).cloned().unwrap_or_default(),
                    have.get(&p).cloned().unwrap_or_default(),
                    "{} port {}", side, p
                );
            }
            // Every scheduled command, exactly once, on its port and frame.
            let mut landed: Vec<Landing> = got
                .iter()
                .filter(|e| e.1 >= SCHEDULED)
                .map(|&(p, tag, _, at)| (p, tag, at))
                .collect();
            landed.sort_by_key(|l| l.1);
            prop_assert_eq!(&landed, &landings, "{} scheduled commands", side);
        }
    }
}

/// A delay whose key vanishes flushes its pending events rather than losing
/// them — the reviewers' counterexample: an emitter behind a 20-frame event
/// delay; regenerate the lag at latency 0, and nothing is lost.
///
/// Mutation: skip the `gone` flush loop in `Executor::rebuild` → 5 of the
/// emitter's events vanish → fails.
#[test]
fn a_vanishing_event_delay_flushes_instead_of_dropping() {
    let emitters = vec![(1u32, 4u64)];
    let base = Phase {
        lag: 20,
        lag_gen: 0,
        emitter_gen: vec![0],
        edges: vec![vec![(0, false)], vec![], vec![]],
        blocks: vec![32; 3],
        commands: Vec::new(),
    };
    let zero = Phase {
        lag: 0,
        lag_gen: 1,
        ..base.clone()
    };
    let phases = vec![base, zero];
    let (emitted, got) = run(&phases, &emitters, true);
    let want = expected(&phases, &emitted, &emitters);
    assert_eq!(delivered(&got), want);
    // And some of them really were in flight across the edit: delivered at
    // or after the switch, emitted before it.
    let switch = 96;
    assert!(got.iter().any(|&(_, _, f, at)| f < switch && at >= switch));
}

/// B1: 65 and 200 sources into one event port compile (to a tree of merges
/// of at most `MAX_PORTS`), and deliver in `(offset, source order)` — here
/// every source fires on the same frame, so the order is the source order.
///
/// Mutation: in `merge_tree`, merge the runs in reverse order → the first
/// tie goes to source 64 → fails. Mutation: remove the `n <= MAX_PORTS` arm
/// (always one flat merge) → the executor panics on the audio thread → fails.
#[test]
fn wide_event_fan_in_is_a_merge_tree_in_source_order() {
    for width in [65usize, 200] {
        let mut t = Topology::default();
        let mut shapes = Shapes::new();
        for i in 0..width {
            let s = Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY).with_events(0, 1);
            t.nodes.insert(
                NodeKey(i as u64),
                NodeSpec::new("e", s.audio_in, s.audio_out),
            );
            shapes.insert(NodeKey(i as u64), s);
        }
        let rs = Shape::audio(ChannelLayout::MONO, ChannelLayout::EMPTY).with_events(1, 0);
        t.nodes
            .insert(REC, NodeSpec::new("r", rs.audio_in, rs.audio_out));
        shapes.insert(REC, rs);
        let mut g = GraphSpec::new(t);
        // Source order is the REVERSE of key order, so a merge that fell back
        // to key order would be caught too.
        for i in (0..width).rev() {
            g.connect_events(
                EventIn { node: REC, port: 0 },
                EventEdge::Direct(EventOut {
                    node: NodeKey(i as u64),
                    port: 0,
                }),
            );
        }
        let valid = g.validate().unwrap();
        let prep = prepare(MAX);
        let (plan, delta) = compile(&valid, &shapes, &prep, None).expect("wide fan-in compiles");
        let merges = plan
            .ops()
            .iter()
            .filter(|op| matches!(op, tutti_graph::Op::EventMerge { .. }))
            .count();
        // One merge per run of more than one source, then one of the runs.
        let full_runs = width / 64 + usize::from(width % 64 > 1);
        assert_eq!(
            merges,
            full_runs + 1,
            "{width}: runs, then one merge of runs"
        );

        let ledger: Ledger = Arc::default();
        let inbox: Inbox = Arc::default();
        let mut units: BTreeMap<NodeKey, Box<dyn Node>> = BTreeMap::new();
        let stop = Arc::new(AtomicBool::new(false));
        for i in 0..width {
            // `id % period == 0` for every id: all fire at frame 0, so the
            // delivery order is decided by source order alone.
            units.insert(
                NodeKey(i as u64),
                Box::new(Emitter {
                    id: i as u32 * 1000,
                    period: 1000,
                    ledger: Arc::clone(&ledger),
                    stop: Arc::clone(&stop),
                }),
            );
        }
        units.insert(
            REC,
            Box::new(Recorder {
                ports: 1,
                inbox: Arc::clone(&inbox),
            }),
        );
        let (mut ed, mut exec) = Editor::with_event_capacity(prep, 4096);
        ed.package(plan, delta, units).expect("room in the queue");
        exec.apply_pending();
        ed.collect();
        exec.process(MAX, &Transport::default(), &[], &mut []);
        let got: Vec<u32> = inbox
            .lock()
            .unwrap()
            .iter()
            .map(|&(_, id, _, _)| id / 1000)
            .collect();
        let want: Vec<u32> = (0..width as u32).rev().collect();
        assert_eq!(
            got, want,
            "{width} sources, all at offset 0, in source order"
        );
        assert_eq!(exec.dropped_events(), 0);
    }
}

/// Outputs the absolute frame index; feeds a feedback edge.
struct Clock;

impl Node for Clock {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO).with_events(0, 1)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let start = cx.env.frame.get();
        for (i, o) in io.output(0).iter_mut().enumerate() {
            *o = (start + i as u64) as f32;
        }
        for at in cx.env.offsets() {
            let f = cx.env.frame_at(at).get();
            io.event_out(0)
                .push(Event::midi(at, [0, f as u32, 0, 0]))
                .expect("one per frame fits");
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// Passes its audio input through and logs its events' emit and delivery
/// frames.
struct Tap(Inbox);

impl Node for Tap {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO)
            .with_events(1, 0)
            .with_tail(Tail::Unknown)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        for e in io.events(0) {
            let EventKind::Midi(Ump(w)) = e.kind else {
                unreachable!()
            };
            self.0
                .lock()
                .unwrap()
                .push((0, 0, u64::from(w[1]), cx.env.frame_at(e.offset).get()));
        }
        io.channel(0).map(|x| x);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// A feedback edge delays by exactly the `delay` its edge declares — audio
/// and events — whatever the block sizes and **whatever `MaxBlock` the
/// interpreter was prepared with**: a bounce prepared at a larger block loops
/// exactly like live playback. Nothing is lost or repeated when the block
/// size changes or is ragged.
///
/// Mutation: in `AudioRing::peek_oldest`, read the newest samples → wrong
/// delay → fails. Mutation: size the executor's feedback ring from
/// `MaxBlock` instead of the key's delay (the previous rule) → the two
/// preparations disagree → fails.
#[test]
fn feedback_delays_by_its_declared_delay_at_any_max_block() {
    const DELAY: usize = 96;
    let (clock, tap) = (NodeKey(1), NodeKey(2));
    for (max, schedule) in [
        (64, vec![64, 64, 30, 30, 30, 64, 1, 7, 64, 64]),
        (64, vec![1, 2, 3, 5, 8, 13, 21, 34, 55, 64, 17, 9, 64, 64]),
        (96, vec![96, 96, 50, 96, 3, 96, 96]),
    ] {
        for executor in [true, false] {
            let inbox: Inbox = Arc::default();
            let mut t = Topology::default();
            let cs = Clock.shape();
            let ts = Tap(Arc::default()).shape();
            t.nodes
                .insert(clock, NodeSpec::new("c", cs.audio_in, cs.audio_out));
            t.nodes.insert(
                tap,
                NodeSpec::new("t", ts.audio_in, ts.audio_out).with_tail(Tail::Unknown),
            );
            t.edges.insert(
                InPort { node: tap, port: 0 },
                Edge::Feedback(FeedbackFrom::new(
                    OutPort {
                        node: clock,
                        port: 0,
                    },
                    Samples(DELAY),
                )),
            );
            t.outputs = vec![Source::Node(OutPort { node: tap, port: 0 })];
            let mut g = GraphSpec::new(t);
            g.connect_events(
                EventIn { node: tap, port: 0 },
                EventEdge::feedback(
                    EventOut {
                        node: clock,
                        port: 0,
                    },
                    Samples(DELAY),
                ),
            );
            let valid = g.validate().unwrap();
            let shapes: Shapes = [(clock, cs), (tap, ts)].into();
            let prep = prepare(max);
            let units = || -> BTreeMap<NodeKey, Box<dyn Node>> {
                [
                    (clock, Box::new(Clock) as Box<dyn Node>),
                    (tap, Box::new(Tap(Arc::clone(&inbox)))),
                ]
                .into()
            };
            let (mut ed, mut exec) = Editor::with_event_capacity(prep, 256);
            let mut reference = Reference::new(prep);
            if executor {
                let (plan, delta) = compile(&valid, &shapes, &prep, None).unwrap();
                ed.package(plan, delta, units()).expect("room in the queue");
                exec.apply_pending();
                ed.collect();
            } else {
                reference.set_graph(&valid, units());
            }
            let mut audio = Vec::new();
            for &n in &schedule {
                let mut out = vec![0.0f32; n];
                if executor {
                    exec.process(n, &Transport::default(), &[], &mut [&mut out[..]]);
                } else {
                    reference.process(n, &Transport::default(), &[], &mut [&mut out[..]]);
                }
                audio.extend(out);
            }
            let total = audio.len();
            let want: Vec<f32> = (0..total)
                .map(|i| if i < DELAY { 0.0 } else { (i - DELAY) as f32 })
                .collect();
            assert_eq!(
                audio, want,
                "audio, max {max}, {schedule:?}, executor {executor}"
            );
            let got = inbox.lock().unwrap().clone();
            let want_ev: Vec<(u64, u64)> = (0..(total as u64).saturating_sub(DELAY as u64))
                .map(|f| (f, f + DELAY as u64))
                .collect();
            let got_ev: Vec<(u64, u64)> = got.iter().map(|&(_, _, e, d)| (e, d)).collect();
            assert_eq!(
                got_ev, want_ev,
                "events, max {max}, {schedule:?}, executor {executor}"
            );
        }
    }
}

/// A feedback delay shorter than the maximum block is refused, by name: a
/// block cannot read samples it has not produced, and shortening the loop to
/// fit would make the bounce sound unlike playback.
///
/// Mutation: delete the `f.delay < max_block` check in `compile` → compiles
/// (and the ring is too short to read a whole block from) → fails.
#[test]
fn a_feedback_delay_shorter_than_max_block_is_refused() {
    let (clock, tap) = (NodeKey(1), NodeKey(2));
    let mut t = Topology::default();
    let cs = Clock.shape();
    let ts = Tap(Arc::default()).shape();
    t.nodes
        .insert(clock, NodeSpec::new("c", cs.audio_in, cs.audio_out));
    t.nodes.insert(
        tap,
        NodeSpec::new("t", ts.audio_in, ts.audio_out).with_tail(Tail::Unknown),
    );
    t.edges.insert(
        InPort { node: tap, port: 0 },
        Edge::Feedback(FeedbackFrom::new(
            OutPort {
                node: clock,
                port: 0,
            },
            Samples(96),
        )),
    );
    let valid = GraphSpec::new(t).validate().unwrap();
    let shapes: Shapes = [(clock, cs), (tap, ts)].into();
    assert!(compile(&valid, &shapes, &prepare(96), None).is_ok());
    assert_eq!(
        compile(&valid, &shapes, &prepare(128), None).err(),
        Some(tutti_graph::CompileError::FeedbackTooShort {
            edge: tutti_graph::CycleEdge::Audio(InPort { node: tap, port: 0 }),
            delay: Samples(96),
            max_block: Samples(128),
        })
    );
}

/// B1 against the reference: a 150-wide fan-in, sources firing on shared and
/// distinct frames, delivers exactly what the reference's
/// concatenate-and-stable-sort delivers.
///
/// Mutation: in `merge_tree`, merge the runs in reverse order → fails.
#[test]
fn wide_fan_in_matches_the_reference() {
    let width = 150usize;
    let mut t = Topology::default();
    let mut shapes = Shapes::new();
    for i in 0..width {
        let s = Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY).with_events(0, 1);
        t.nodes.insert(
            NodeKey(i as u64),
            NodeSpec::new("e", s.audio_in, s.audio_out),
        );
        shapes.insert(NodeKey(i as u64), s);
    }
    let rs = Shape::audio(ChannelLayout::MONO, ChannelLayout::EMPTY).with_events(1, 0);
    t.nodes
        .insert(REC, NodeSpec::new("r", rs.audio_in, rs.audio_out));
    shapes.insert(REC, rs);
    let mut g = GraphSpec::new(t);
    for i in (0..width).rev() {
        g.connect_events(
            EventIn { node: REC, port: 0 },
            EventEdge::Direct(EventOut {
                node: NodeKey(i as u64),
                port: 0,
            }),
        );
    }
    let valid = g.validate().unwrap();
    let prep = prepare(MAX);
    let run = |executor: bool| {
        let ledger: Ledger = Arc::default();
        let inbox: Inbox = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let mut units: BTreeMap<NodeKey, Box<dyn Node>> = BTreeMap::new();
        for i in 0..width {
            units.insert(
                NodeKey(i as u64),
                Box::new(Emitter {
                    id: i as u32,
                    period: 4 + (i as u64 % 3),
                    ledger: Arc::clone(&ledger),
                    stop: Arc::clone(&stop),
                }),
            );
        }
        units.insert(
            REC,
            Box::new(Recorder {
                ports: 1,
                inbox: Arc::clone(&inbox),
            }),
        );
        let (mut ed, mut exec) = Editor::with_event_capacity(prep, 4096);
        let mut reference = Reference::new(prep);
        if executor {
            let (plan, delta) = compile(&valid, &shapes, &prep, None).unwrap();
            ed.package(plan, delta, units).expect("room in the queue");
            exec.apply_pending();
            ed.collect();
        } else {
            reference.set_graph(&valid, units);
        }
        for n in [MAX, 7, MAX, 1, 20] {
            if executor {
                exec.process(n, &Transport::default(), &[], &mut []);
            } else {
                reference.process(n, &Transport::default(), &[], &mut []);
            }
        }
        let got = inbox.lock().unwrap().clone();
        got
    };
    let (a, b) = (run(true), run(false));
    assert!(a.len() > width, "the sources fired");
    assert_eq!(a, b);
}

/// A flush keeps its events' spacing: a vanished 40-frame delay holding
/// events 4 frames apart delivers them 4 frames apart in the next block —
/// a note-on/note-off pair keeps its length instead of collapsing to zero —
/// and only what falls past the block end is clamped to its last frame.
///
/// Mutation: make `EventFifo::flushed` put every event at offset 0 (the old
/// rule) → the spacing collapses → fails.
#[test]
fn a_flush_keeps_the_events_spacing() {
    let emitters = vec![(1u32, 4u64)];
    let base = Phase {
        lag: 40,
        lag_gen: 0,
        emitter_gen: vec![0],
        edges: vec![vec![(0, false)], vec![], vec![]],
        blocks: vec![32; 3],
        commands: Vec::new(),
    };
    let zero = Phase {
        lag: 0,
        lag_gen: 1,
        ..base.clone()
    };
    let (_, got) = run(&[base, zero], &emitters, true);
    let switch = 96u64;
    let flushed: Vec<(u64, u64)> = got
        .iter()
        .filter(|&&(_, _, f, at)| f < switch && at >= switch && at < switch + 32)
        .map(|&(_, _, f, at)| (f, at))
        .collect();
    assert!(
        flushed.len() >= 4,
        "several events were in flight: {flushed:?}"
    );
    let spaced: Vec<&(u64, u64)> = flushed
        .iter()
        .filter(|&&(_, at)| at < switch + 31)
        .collect();
    for w in spaced.windows(2) {
        assert_eq!(
            w[1].1 - w[0].1,
            w[1].0 - w[0].0,
            "flushed events keep their spacing: {flushed:?}"
        );
    }
    assert_eq!(spaced[0].1, switch, "the earliest lands at offset 0");
}

/// Two flushes into one sink interleave by their spacing, the same way in
/// both interpreters — including when one-frame blocks clamp them all to
/// offset 0, where only the order is left to disagree about. (The
/// conservation property compares sets; this compares order.)
///
/// Mutation: in `Reference::set_graph`, drop the sort after appending a
/// flush → the reference delivers one FIFO's events, then the other's →
/// fails.
#[test]
fn two_flushes_into_one_sink_interleave_identically() {
    let emitters = vec![(1u32, 10u64), (2u32, 10u64)];
    let fed = Phase {
        lag: 0,
        lag_gen: 0,
        emitter_gen: vec![0, 0],
        edges: vec![vec![(0, true), (1, true)], vec![], vec![]],
        blocks: vec![32; 6],
        commands: Vec::new(),
    };
    let cut = Phase {
        edges: vec![vec![], vec![], vec![]],
        blocks: vec![1; 40],
        ..fed.clone()
    };
    let phases = [fed, cut];
    let (_, a) = run(&phases, &emitters, true);
    let (_, b) = run(&phases, &emitters, false);
    let ids: Vec<u32> = a.iter().filter(|e| e.3 >= 192).map(|e| e.1).collect();
    assert!(
        ids.windows(2).any(|w| w[0] != w[1]),
        "the flushed events interleave: {ids:?}"
    );
    assert_eq!(a, b);
}
