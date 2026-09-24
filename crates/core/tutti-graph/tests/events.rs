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
//! - **Feedback is exactly `MaxBlock` frames** (S2), audio and events, under
//!   block sizes that change and are ragged.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use common::prepare;
use proptest::prelude::*;
use tutti_graph::{
    compile, Commit, Cx, Editor, Event, EventEdge, EventIn, EventKind, EventOut, Executor,
    GraphSpec, Io, Node, Prepare, Reference, Shape, Shapes, Status, Transport, Ump,
};
use tutti_types::graph::{Edge, FeedbackFrom, InPort, NodeSpec, OutPort, Source};
use tutti_types::{ChannelLayout, Latency, NodeKey, Samples, Tail, Topology};

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
        let start = cx.env.frame;
        if self.stop.load(Ordering::Relaxed) {
            return Status::Silent;
        }
        for i in 0..io.frames() as u64 {
            let f = start + i;
            if f % self.period == u64::from(self.id) % self.period {
                io.event_out(0)
                    .push(Event::midi(
                        i as u32,
                        [self.id, f as u32, (f >> 32) as u32, 0],
                    ))
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
                inbox.push((p as u16, w[0], emitted, cx.env.frame + u64::from(e.offset)));
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
}

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
                    EventEdge::Feedback(from)
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
fn run(
    phases: &[Phase],
    emitters: &[(u32, u64)],
    executor: bool,
) -> (Vec<(usize, u32, u64)>, Vec<(u16, u32, u64, u64)>) {
    let ledger: Ledger = Arc::default();
    let inbox: Inbox = Arc::default();
    let stop = Arc::new(AtomicBool::new(false));
    let prep = prepare(MAX);
    let (_ed, mut exec): (Editor, Executor) = Editor::with_event_capacity(prep, CAP);
    let mut reference = Reference::new(prep);
    let mut prev = None;
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
            let (plan, delta) = compile(&valid, &shapes, &prep, prev.as_ref()).expect("compiles");
            let placed: BTreeSet<NodeKey> = delta
                .insert
                .iter()
                .map(|p| p.key)
                .chain(delta.replace.iter().map(|(_, n)| n.key))
                .collect();
            let mut units = units_of(ph, emitters, &ledger, &inbox, &stop);
            units.retain(|k, _| placed.contains(k));
            prev = Some(plan.clone());
            drop(exec.apply(Commit::new(plan, delta, units)));
        } else {
            reference.set_graph(&valid, units_of(ph, emitters, &ledger, &inbox, &stop));
        }
        let before = ledger.lock().unwrap().len();
        for &n in &ph.blocks {
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
    }
    let got = inbox.lock().unwrap().clone();
    (phase_of, got)
}

/// The drain phase: the last phase's wiring (so nothing is recompiled away),
/// emitters stopped, and blocks enough for every delay to empty.
fn ph_drain(phases: &[Phase]) -> Phase {
    let last = phases.last().expect("at least one phase").clone();
    Phase {
        blocks: vec![MAX; 8],
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
    for &(p, id, f, _) in got {
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
    )
        .prop_map(|(lag, lag_gen, emitter_gen, edges, blocks)| Phase {
            lag,
            lag_gen,
            emitter_gen,
            edges: edges.into_iter().map(|m| m.into_iter().collect()).collect(),
            blocks,
        })
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
    #[test]
    fn every_event_is_delivered_exactly_once(
        phases in proptest::collection::vec(arb_phase(4), 1..4),
    ) {
        let emitters: Vec<(u32, u64)> = vec![(1, 3), (2, 5), (3, 7), (4, 4)];
        for executor in [true, false] {
            let (emitted, got) = run(&phases, &emitters, executor);
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
        let (_ed, mut exec) = Editor::with_event_capacity(prep, 4096);
        drop(exec.apply(Commit::new(plan, delta, units)));
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
        let start = cx.env.frame;
        for (i, o) in io.output(0).iter_mut().enumerate() {
            *o = (start + i as u64) as f32;
        }
        for i in 0..io.frames() {
            let f = start + i as u64;
            io.event_out(0)
                .push(Event::midi(i as u32, [0, f as u32, 0, 0]))
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
            self.0.lock().unwrap().push((
                0,
                0,
                u64::from(w[1]),
                cx.env.frame + u64::from(e.offset),
            ));
        }
        io.channel(0).map(|x| x);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// S2: a feedback edge delays by exactly `MaxBlock` frames — audio and
/// events — when the block size changes (64 then 30) and when it is ragged.
/// Nothing is lost, nothing repeats.
///
/// Mutation: in `AudioRing::peek_oldest`, read the newest samples → wrong
/// delay → fails. Mutation: capture audio feedback as "last block's buffer"
/// (the old rule) → samples are lost when the block shrinks → fails.
#[test]
fn feedback_delays_by_exactly_max_block_under_changing_blocks() {
    let (clock, tap) = (NodeKey(1), NodeKey(2));
    for schedule in [
        vec![64, 64, 30, 30, 30, 64, 1, 7, 64],
        vec![1, 2, 3, 5, 8, 13, 21, 34, 55, 64, 17, 9],
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
                Edge::Feedback(FeedbackFrom {
                    from: OutPort {
                        node: clock,
                        port: 0,
                    },
                }),
            );
            t.outputs = vec![Source::Node(OutPort { node: tap, port: 0 })];
            let mut g = GraphSpec::new(t);
            g.connect_events(
                EventIn { node: tap, port: 0 },
                EventEdge::Feedback(EventOut {
                    node: clock,
                    port: 0,
                }),
            );
            let valid = g.validate().unwrap();
            let shapes: Shapes = [(clock, cs), (tap, ts)].into();
            let prep = prepare(64);
            let units = || -> BTreeMap<NodeKey, Box<dyn Node>> {
                [
                    (clock, Box::new(Clock) as Box<dyn Node>),
                    (tap, Box::new(Tap(Arc::clone(&inbox)))),
                ]
                .into()
            };
            let (_ed, mut exec) = Editor::with_event_capacity(prep, 256);
            let mut reference = Reference::new(prep);
            if executor {
                let (plan, delta) = compile(&valid, &shapes, &prep, None).unwrap();
                drop(exec.apply(Commit::new(plan, delta, units())));
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
                .map(|i| if i < 64 { 0.0 } else { (i - 64) as f32 })
                .collect();
            assert_eq!(
                audio, want,
                "audio, schedule {schedule:?}, executor {executor}"
            );
            let got = inbox.lock().unwrap().clone();
            let want_ev: Vec<(u64, u64)> = (0..(total as u64).saturating_sub(64))
                .map(|f| (f, f + 64))
                .collect();
            let got_ev: Vec<(u64, u64)> = got.iter().map(|&(_, _, e, d)| (e, d)).collect();
            assert_eq!(
                got_ev, want_ev,
                "events, schedule {schedule:?}, executor {executor}"
            );
        }
    }
}
