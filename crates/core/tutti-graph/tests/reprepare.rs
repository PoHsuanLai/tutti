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
/// among those the emitter made in `spans` (its clock's frames — the emitter
/// fires on every seventh one; a rate change jumps the clock between spans).
fn assert_direct_events_conserved(inbox: &Inbox, spans: &[std::ops::Range<u64>]) {
    let got: Vec<u32> = inbox
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e.1 == 0)
        .map(|e| e.0)
        .collect();
    let unique: BTreeSet<u32> = got.iter().copied().collect();
    assert_eq!(unique.len(), got.len(), "no event delivered twice");
    for f in spans.iter().cloned().flatten().filter(|f| f % 7 == 0) {
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
    let span = 0..got.len() as u64 - 64;
    assert_direct_events_conserved(&rig.exec_inbox, std::slice::from_ref(&span));
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
    // The clock doubles at the change (192 → 384): the same wall-clock time
    // at twice the rate.
    assert_eq!(rig.exec.frame().get(), 384 + 256);
    assert_direct_events_conserved(&rig.exec_inbox, &[0..192, 384..384 + 256 - 64]);
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

/// Between the two commits the executor renders silence for whatever block
/// the device hands it — here one as long as the new, larger maximum —
/// because its units are on the control thread.
///
/// Mutation: in `Executor::process`, check the block bound before the
/// suspended branch → the 128-frame block panics against the old 64 →
/// fails.
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
    assert_eq!(
        rig.exec.frame().get(),
        256,
        "the clock tracks device time, silent blocks included"
    );
    assert_eq!(rig.ed.commit(), Ok(()), "collects, resumes, then commits");
    rig.exec.process(
        128,
        &Transport::default(),
        &[&input[..]],
        &mut [&mut out[..]],
    );
    assert!(out.iter().any(|&x| x != 0.0), "running again");
}

/// Shrinking `MaxBlock`: while suspended, the device may still hand blocks
/// sized for the old maximum. They render as silence rather than panic, and
/// the new maximum is adopted only when the resume commit lands.
///
/// Mutation: adopt the new `Prepare` when the suspend lands (the old rule:
/// `self.prepare = prepare` in `Executor::apply`'s suspend branch) → the
/// "not yet adopted" check fails, and with the bound checked before the
/// suspended branch the 256-frame block panics against 64.
#[test]
fn shrinking_max_block_never_fails_the_callback() {
    let mut rig = rig(prep(48_000.0, 256));
    rig.render(&[256, 256]);
    rig.ed.reprepare(prep(48_000.0, 64)).expect("reprepares");
    let input = vec![1.0f32; 256];
    let mut out = vec![9.0f32; 256];
    let t = Transport::default();
    rig.exec
        .process(256, &t, &[&input[..]], &mut [&mut out[..]]);
    assert!(out.iter().all(|&x| x == 0.0), "silence at the old size");
    assert_eq!(rig.exec.prepare().max_block().get(), 256, "not yet adopted");
    rig.ed.collect(); // sends the resume
    let mut out = vec![0.0f32; 64];
    rig.exec
        .process(64, &t, &[&input[..64]], &mut [&mut out[..]]);
    assert_eq!(rig.exec.prepare().max_block().get(), 64);
    assert!(
        out.iter().any(|&x| x != 0.0),
        "running again at the new size"
    );
}

/// A node that panics in `prepare` at 96 kHz.
struct FragileAt96k;

impl Node for FragileAt96k {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
    }
    fn prepare(&mut self, p: &Prepare) {
        assert!(p.sample_rate().get() < 90_000.0, "cannot run this fast");
    }
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(1.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// A node that honours event offsets sample-accurately only below 96 kHz.
struct CoarseAt96k(tutti_graph::Resolution);

impl Node for CoarseAt96k {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY)
            .with_events(1, 0)
            .with_event_resolution(self.0)
    }
    fn prepare(&mut self, p: &Prepare) {
        self.0 = if p.sample_rate().get() < 90_000.0 {
            tutti_graph::Resolution::Sample
        } else {
            tutti_graph::Resolution::Block
        };
    }
    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// Drive `ed`/`exec` through a failing re-prepare and check the poisoned
/// state: every call refuses, naming `cause`; the executor renders silence
/// for any block, forever, and never panics.
fn assert_poisoned(mut ed: Editor, mut exec: Executor, cause: &str) {
    ed.reprepare(prep(96_000.0, 64))
        .expect("the first half succeeds");
    exec.apply_pending();
    ed.collect(); // the second half fails here — and must not unwind
    match ed.commit() {
        Err(CommitError::Poisoned { cause: c }) => assert!(c.contains(cause), "{c}"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        ed.reprepare(prep(48_000.0, 64)),
        Err(CommitError::Poisoned { .. })
    ));
    assert_eq!(
        ed.schedule(
            tutti_types::At::NextBlock,
            EventIn {
                node: NodeKey(1),
                port: 0
            },
            EventKind::Midi(Ump([0; 4]))
        ),
        Err(tutti_graph::ScheduleError::Poisoned)
    );
    assert_eq!(ed.cancel_all(), Err(tutti_graph::ScheduleError::Poisoned));
    let t = Transport::default();
    for n in [64usize, 256, 1, 1024] {
        let mut out = vec![9.0f32; n];
        exec.process(n, &t, &[], &mut [&mut out[..]]);
        assert!(out.iter().all(|&x| x == 0.0), "silence, for any block");
    }
}

/// A node panicking in `prepare` after the units are out poisons the editor
/// instead of unwinding out of `collect` and stranding the executor.
///
/// Mutation: drop the `catch_unwind` in `Editor::resume` (call
/// `finish_reprepare` directly) → the panic escapes `collect` → fails.
#[test]
fn a_panic_in_prepare_poisons_the_editor() {
    let (mut ed, mut exec) = Editor::new(prep(48_000.0, 64));
    ed.insert(NodeKey(1), "fragile", FragileAt96k);
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort {
        node: NodeKey(1),
        port: 0,
    })];
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    assert_poisoned(ed, exec, "panicked");
}

/// Re-prepared shapes that no longer compile — an event resolution that
/// coarsens under a marked edge — poison the editor too, and say why.
///
/// Mutation: in `Editor::finish_reprepare`, `expect` the compile instead of
/// returning its error → the cause reads "panicked", not "no longer
/// compile" → fails.
#[test]
fn a_resolution_that_coarsens_on_prepare_poisons_the_editor() {
    let (mut ed, mut exec) = Editor::new(prep(48_000.0, 64));
    ed.insert(
        NodeKey(0),
        "emit",
        TestNode::new(Kind::Emitter {
            period: 5,
            phase: 0,
        }),
    );
    ed.insert(
        NodeKey(1),
        "coarse",
        CoarseAt96k(tutti_graph::Resolution::Sample),
    );
    let (from, at) = (
        EventOut {
            node: NodeKey(0),
            port: 0,
        },
        EventIn {
            node: NodeKey(1),
            port: 0,
        },
    );
    ed.spec_mut().connect_events(at, EventEdge::Direct(from));
    ed.spec_mut()
        .require_resolution(at, from, tutti_graph::Resolution::Sample);
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort {
        node: NodeKey(0),
        port: 0,
    })];
    ed.commit().expect("sample-accurate at 48 kHz");
    exec.apply_pending();
    ed.collect();
    assert_poisoned(ed, exec, "no longer compile");
}

/// `Frame` means samples at the current rate. On a rate change the clock and
/// every pending frame-timed command scheduled before the re-prepare move to
/// the same wall-clock time at the new rate (nearest frame); one scheduled
/// after it already speaks the new rate. Blocks rendered while suspended
/// count on the clock. Checked in both interpreters, with a suspended block
/// between the two halves.
///
/// Mutation: skip `CommandRx::rescale` in the suspend → the old-rate command
/// lands at 1 030 instead of 2 030 → fails. Mutation: rescale every pending
/// command, ignoring `before` → the new-rate one lands at 3 030 → fails.
/// Mutation: stop the executor's clock while suspended → it lands 64 frames
/// off against the reference, and the clock check fails.
#[test]
fn a_rate_change_keeps_pending_commands_at_their_wall_clock_time() {
    let mut rig = rig(prep(48_000.0, 64));
    rig.render(&[64, 64, 64]);
    let to = EventIn { node: REC, port: 0 };
    let note = |t: u32| EventKind::Midi(Ump([t, 9, 0, 0]));
    let old_rate = tutti_types::At::Frame(tutti_types::Frame(1_000));
    rig.ed.schedule(old_rate, to, note(70_000)).expect("room");
    rig.reference.schedule(old_rate, to, note(70_000));

    let p = prep(96_000.0, 64);
    rig.ed.reprepare(p).expect("reprepares");
    rig.reference.suspend(p);
    let new_rate = tutti_types::At::Frame(tutti_types::Frame(1_500));
    rig.ed.schedule(new_rate, to, note(70_001)).expect("room");
    rig.reference.schedule(new_rate, to, note(70_001));
    // One block while suspended: silence in both, and the clock counts.
    rig.render(&[64]);
    assert_eq!(
        rig.exec.frame().get(),
        384 + 64,
        "192 frames at 48 kHz is 384 at 96"
    );
    rig.ed.collect(); // the resume
    rig.reference.resume();
    rig.render(&[64; 30]);
    for inbox in [&rig.exec_inbox, &rig.ref_inbox] {
        let got: Vec<(u32, u64)> = inbox
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.0 >= 70_000)
            .map(|e| (e.0, e.2))
            .collect();
        // The recorder's arrival is 30 frames (the EventLag path): both land
        // 30 after their timeline frames.
        assert_eq!(got, vec![(70_001, 1_530), (70_000, 2_030)]);
    }
    assert_eq!(rig.exec.late_commands(), 0);
}
