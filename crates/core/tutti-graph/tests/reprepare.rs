//! `Editor::reprepare`: a sample-rate or `MaxBlock` change on a running graph
//! — every unit re-prepared on the control thread, the plan recompiled, and
//! delay and feedback state kept or reset by the documented rule (a rate
//! change resets time-based state and flushes pending events; a `MaxBlock`
//! change keeps everything).

mod common;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use common::{Kind, TestNode};
use tutti_graph::{
    CommitError, CompileError, Cx, CycleEdge, Editor, EventEdge, EventIn, EventKind, EventOut,
    Executor, Io, Node, Prepare, Reference, Shape, Status, Transport, Ump,
};
use tutti_types::graph::{Edge, FeedbackFrom, InPort, OutPort, Source};
use tutti_types::{ChannelLayout, Latency, NodeKey, SampleRate, Samples, Tail};

fn prep(rate: f64, max: usize) -> Prepare {
    Prepare::new(SampleRate(rate), Samples(max))
}

const LAG: NodeKey = NodeKey(1);
const SUM: NodeKey = NodeKey(2);
const EMIT: NodeKey = NodeKey(3);
const EVLAG: NodeKey = NodeKey(4);
const REC: NodeKey = NodeKey(5);

/// `(tag word 0, hop word 1, delivery frame)` per event received.
type Inbox = Arc<Mutex<Vec<(u32, u32, u64)>>>;

/// Logs every event it receives, with its absolute delivery frame.
struct Recorder(Inbox);

impl Node for Recorder {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY)
            .with_events(1, 0)
            .with_tail(Tail::Unknown)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
        for e in io.events(0) {
            if let EventKind::Midi(Ump(w)) = e.kind {
                self.0
                    .lock()
                    .unwrap()
                    .push((w[0], w[1], cx.env.frame_at(e.offset).get()));
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// The graph under test, as units:
///
/// ```text
/// global ─┬─ Lag(20) ─── Sum.0 ── out 0
///         └──────────── Sum.1        (a 20-frame PDC ring on Sum.1)
/// Emit ─┬─ EventLag(30) ─ Rec        (Rec's direct edge from Emit gets a
///       └─────────────── Rec          30-frame event delay)
/// ```
fn units(inbox: &Inbox) -> Vec<(NodeKey, Box<dyn Node>)> {
    vec![
        (LAG, Box::new(TestNode::new(Kind::Lag { latency: 20 }))),
        (SUM, Box::new(TestNode::new(Kind::Sum { inputs: 2 }))),
        (
            EMIT,
            Box::new(TestNode::new(Kind::Emitter {
                period: 7,
                phase: 0,
            })),
        ),
        (
            EVLAG,
            Box::new(TestNode::new(Kind::EventLag { latency: 30 })),
        ),
        (REC, Box::new(Recorder(Arc::clone(inbox)))),
    ]
}

fn wire(ed: &mut Editor) {
    let spec = ed.spec_mut();
    spec.topology.inputs = ChannelLayout::MONO;
    let t = &mut spec.topology;
    t.edges.insert(
        InPort { node: LAG, port: 0 },
        Edge::Direct(Source::Global(0)),
    );
    t.edges.insert(
        InPort { node: SUM, port: 0 },
        Edge::Direct(Source::Node(OutPort { node: LAG, port: 0 })),
    );
    t.edges.insert(
        InPort { node: SUM, port: 1 },
        Edge::Direct(Source::Global(0)),
    );
    t.outputs = vec![Source::Node(OutPort { node: SUM, port: 0 })];
    let emit = EventOut {
        node: EMIT,
        port: 0,
    };
    spec.connect_events(
        EventIn {
            node: EVLAG,
            port: 0,
        },
        EventEdge::Direct(emit),
    );
    for from in [
        EventOut {
            node: EVLAG,
            port: 0,
        },
        emit,
    ] {
        spec.connect_events(EventIn { node: REC, port: 0 }, EventEdge::Direct(from));
    }
}

/// An editor/executor pair and a reference over the graph above, both at
/// `p`, logging events into separate inboxes.
struct Rig {
    ed: Editor,
    exec: Executor,
    reference: Reference,
    exec_inbox: Inbox,
    ref_inbox: Inbox,
}

fn rig(p: Prepare) -> Rig {
    let (exec_inbox, ref_inbox) = (Inbox::default(), Inbox::default());
    let (mut ed, mut exec) = Editor::new(p);
    for (k, u) in units(&exec_inbox) {
        ed.insert(k, "n", u);
    }
    wire(&mut ed);
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    let mut reference = Reference::new(p);
    reference.set_graph(
        &ed.spec().validate().unwrap(),
        units(&ref_inbox).into_iter().collect(),
    );
    Rig {
        ed,
        exec,
        reference,
        exec_inbox,
        ref_inbox,
    }
}

impl Rig {
    /// Re-prepare both, with no block rendered between the two commits.
    fn reprepare(&mut self, p: Prepare) {
        self.ed.reprepare(p).expect("reprepares");
        self.exec.apply_pending(); // checks the units out
        self.ed.collect(); // re-prepares them, sends them back
        self.exec.apply_pending(); // installs them
        assert_eq!(self.ed.in_flight(), 1, "the resume box is out");
        self.ed.collect();
        self.reference.reprepare(p);
    }

    /// Render `blocks` through both on a constant 1.0 input; the executor's
    /// output, after asserting the reference's is bit-identical.
    fn render(&mut self, blocks: &[usize]) -> Vec<f32> {
        let mut all = Vec::new();
        let t = Transport::default();
        for &n in blocks {
            let input = vec![1.0f32; n];
            let (mut a, mut b) = (vec![0.0f32; n], vec![0.0f32; n]);
            self.exec.process(n, &t, &[&input[..]], &mut [&mut a[..]]);
            self.reference
                .process(n, &t, &[&input[..]], &mut [&mut b[..]]);
            assert_eq!(common::bits(&[a.clone()]), common::bits(&[b]));
            all.extend(a);
        }
        all
    }
}

/// Every direct emission (hop 0) delivered exactly once, and none missing
/// among those emitted early enough to have landed by `end`.
fn assert_direct_events_conserved(inbox: &Inbox, end: u64) {
    let got: Vec<u32> = inbox
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e.1 == 0)
        .map(|e| e.0)
        .collect();
    let unique: BTreeSet<u32> = got.iter().copied().collect();
    assert_eq!(unique.len(), got.len(), "no event delivered twice");
    for f in (0..end.saturating_sub(64)).step_by(7) {
        assert!(
            unique.contains(&(f as u32)),
            "the event emitted at {f} was lost"
        );
    }
}

/// A `MaxBlock`-only change keeps every ring and FIFO: the output is exactly
/// what a graph prepared at the larger block from the start renders, and no
/// event is lost or repeated.
///
/// Mutation: in `Executor::rebuild`, never carry state (`carry = false`) →
/// the PDC ring restarts silent → the output dips → fails. Mutation: in
/// `EventFifo::resize`, drop the queued events (`q.clear()`) → the events in
/// the delay at the change are lost → fails.
#[test]
fn a_max_block_change_keeps_every_ring() {
    let mut rig = rig(prep(48_000.0, 64));
    let mut got = rig.render(&[64, 17, 64, 5, 64]);
    rig.reprepare(prep(48_000.0, 128));
    assert_eq!(rig.exec.prepare().max_block().get(), 128);
    got.extend(rig.render(&[128, 3, 128, 100, 128]));

    // The same graph, at 128 from the start, with the same block schedule.
    let mut straight = rig_reference(prep(48_000.0, 128));
    let want = straight.render(&[64, 17, 64, 5, 64, 128, 3, 128, 100, 128]);
    assert_eq!(common::bits(&[got.clone()]), common::bits(&[want]));
    assert_direct_events_conserved(&rig.exec_inbox, got.len() as u64);
    assert_eq!(
        *rig.exec_inbox.lock().unwrap(),
        *straight.inbox.lock().unwrap(),
        "and every event lands where it would have"
    );
}

/// A reference alone, for a baseline run.
struct Solo {
    reference: Reference,
    inbox: Inbox,
}

fn rig_reference(p: Prepare) -> Solo {
    let inbox = Inbox::default();
    let (mut ed, _exec) = Editor::new(p);
    for (k, u) in units(&Inbox::default()) {
        ed.insert(k, "n", u);
    }
    wire(&mut ed);
    let mut reference = Reference::new(p);
    reference.set_graph(
        &ed.spec().validate().unwrap(),
        units(&inbox).into_iter().collect(),
    );
    Solo { reference, inbox }
}

impl Solo {
    fn render(&mut self, blocks: &[usize]) -> Vec<f32> {
        let mut all = Vec::new();
        for &n in blocks {
            let input = vec![1.0f32; n];
            let mut out = vec![0.0f32; n];
            self.reference
                .process(n, &Transport::default(), &[&input[..]], &mut [&mut out[..]]);
            all.extend(out);
        }
        all
    }
}

/// A sample-rate change resets time-based state: the PDC ring on `Sum.1`
/// restarts silent (for its 20 frames the output is the `Lag` path alone —
/// the node's own state is the node's business and carries), and the events
/// pending in the 30-frame event delay are flushed, not lost.
///
/// Mutation: in `Executor::rebuild`, carry state across a rate change
/// (`carry = true`) → the ring keeps its old-rate audio → the output stays
/// at 2.0 → fails. Mutation: in `Executor::rebuild`'s flush, skip the
/// vanished event delays (`gone` loop) → the events in flight at the change
/// are lost → fails.
#[test]
fn a_rate_change_resets_rings_and_flushes_pending_events() {
    let mut rig = rig(prep(48_000.0, 64));
    let before = rig.render(&[64, 64, 64]);
    assert!(
        before[40..].iter().all(|&x| x == 2.0),
        "both paths carry 1.0"
    );
    let in_flight_at_change = 192u64;
    rig.reprepare(prep(96_000.0, 64));
    assert_eq!(rig.exec.prepare().sample_rate(), SampleRate(96_000.0));
    let after = rig.render(&[64, 64, 64, 64]);
    assert!(
        after[..20].iter().all(|&x| x == 1.0),
        "the ring restarted silent: {:?}",
        &after[..24]
    );
    assert!(after[20..].iter().all(|&x| x == 2.0));
    assert_direct_events_conserved(&rig.exec_inbox, 192 + 256);
    assert_eq!(
        *rig.exec_inbox.lock().unwrap(),
        *rig.ref_inbox.lock().unwrap(),
        "the reference, stating the rule independently, agrees event for event"
    );
    // And some really were in the delay at the change, and landed after it.
    assert!(rig
        .exec_inbox
        .lock()
        .unwrap()
        .iter()
        .any(|&(f, hop, at)| hop == 0
            && u64::from(f) < in_flight_at_change
            && at >= in_flight_at_change));
}

/// Feedback whose delay is shorter than the new `MaxBlock` is refused by
/// name, before anything is sent: the executor keeps running as it was.
///
/// Mutation: in `Editor::reprepare`, skip the pre-compile → the suspend is
/// sent and the resume's compile panics in `collect` → fails.
#[test]
fn a_feedback_edge_shorter_than_the_new_block_is_refused_first() {
    let (mut ed, mut exec) = Editor::new(prep(48_000.0, 64));
    ed.insert(LAG, "smooth", TestNode::new(Kind::Smooth));
    let at = InPort { node: LAG, port: 0 };
    ed.spec_mut().topology.edges.insert(
        at,
        Edge::Feedback(FeedbackFrom::new(
            OutPort { node: LAG, port: 0 },
            Samples(96),
        )),
    );
    ed.commit().expect("96 >= 64");
    exec.apply_pending();
    ed.collect();
    assert_eq!(
        ed.reprepare(prep(48_000.0, 128)),
        Err(CommitError::Compile(CompileError::FeedbackTooShort {
            edge: CycleEdge::Audio(at),
            delay: Samples(96),
            max_block: Samples(128),
        }))
    );
    assert_eq!(ed.in_flight(), 0, "nothing was sent");
    assert_eq!(ed.prepare().max_block().get(), 64);
    exec.process(64, &Transport::default(), &[], &mut []);
    assert_eq!(exec.prepare().max_block().get(), 64);
    assert_eq!(ed.commit(), Ok(()), "and the editor is not stuck");
}

/// Declares 1 ms of latency at whatever rate it was last prepared for, and
/// logs every `Prepare` it is handed.
struct RateLatency {
    latency: usize,
    log: Arc<Mutex<Vec<Prepare>>>,
}

impl Node for RateLatency {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO)
            .with_latency(Latency::new(Samples(self.latency)))
    }
    fn prepare(&mut self, p: &Prepare) {
        self.latency = (p.sample_rate().get() / 1000.0) as usize;
        self.log.lock().unwrap().push(*p);
    }
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.channel(0).map(|x| x);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// Latency is a time for some nodes. Every unit is re-prepared once, with
/// the new `Prepare`, on the control thread — the running ones and one inserted but not yet committed — and
/// the rate-dependent latency reaches the spec and the plan.
///
/// Mutation: in `Editor::resume`, skip `refresh` → the spec still says 48
/// frames, the shape says 96, and the resume's compile panics → fails.
/// Mutation: in `Editor::reprepare`, do not re-prepare the pending units →
/// the uncommitted one keeps its 48-frame shape → fails.
#[test]
fn every_unit_is_re_prepared_and_its_new_latency_compiled() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let unit = |log: &Arc<Mutex<Vec<Prepare>>>| RateLatency {
        latency: 0,
        log: Arc::clone(log),
    };
    let (mut ed, mut exec) = Editor::new(prep(48_000.0, 64));
    ed.spec_mut().topology.inputs = ChannelLayout::MONO;
    ed.insert(LAG, "look", unit(&log));
    ed.spec_mut().topology.edges.insert(
        InPort { node: LAG, port: 0 },
        Edge::Direct(Source::Global(0)),
    );
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: LAG, port: 0 })];
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    assert_eq!(exec.plan().unwrap().total_latency().samples(), Samples(48));

    // Uncommitted: inserted and wired, then the rate changes.
    ed.insert(SUM, "look", unit(&log));
    ed.spec_mut().topology.edges.insert(
        InPort { node: SUM, port: 0 },
        Edge::Direct(Source::Node(OutPort { node: LAG, port: 0 })),
    );
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: SUM, port: 0 })];
    log.lock().unwrap().clear();

    let p = prep(96_000.0, 64);
    ed.reprepare(p).expect("reprepares");
    assert_eq!(ed.commit(), Err(CommitError::Repreparing));
    exec.apply_pending();
    ed.collect();
    exec.apply_pending();
    ed.collect();
    assert_eq!(
        *log.lock().unwrap(),
        vec![p, p],
        "each unit once, with the new Prepare"
    );
    let plan = exec.plan().unwrap();
    assert_eq!(
        plan.total_latency().samples(),
        Samples(192),
        "two 1 ms lookaheads"
    );
    assert_eq!(ed.spec().topology.nodes[&LAG].latency, Samples(96));
    assert_eq!(ed.spec().topology.nodes[&SUM].latency, Samples(96));
    assert_eq!(ed.commit(), Ok(()), "and commits flow again");
}

/// Between the two commits the executor has adopted the new `Prepare` —
/// a block as long as the new maximum is accepted — and renders silence,
/// because its units are on the control thread.
///
/// Mutation: in `Executor::apply`, do not adopt the suspend's `Prepare`
/// → the 128-frame block panics against the old 64 → fails.
#[test]
fn between_the_two_commits_the_executor_renders_silence_at_the_new_size() {
    let mut rig = rig(prep(48_000.0, 64));
    rig.render(&[64, 64]);
    rig.ed.reprepare(prep(48_000.0, 128)).expect("reprepares");
    let input = vec![1.0f32; 128];
    let mut out = vec![9.0f32; 128];
    rig.exec.process(
        128,
        &Transport::default(),
        &[&input[..]],
        &mut [&mut out[..]],
    );
    assert!(out.iter().all(|&x| x == 0.0));
    assert_eq!(rig.exec.frame().get(), 128, "paused: the clock stood still");
    assert_eq!(rig.ed.commit(), Ok(()), "collects, resumes, then commits");
    rig.exec.process(
        128,
        &Transport::default(),
        &[&input[..]],
        &mut [&mut out[..]],
    );
    assert!(out.iter().any(|&x| x != 0.0), "running again");
}
