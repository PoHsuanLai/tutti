//! Timestamped commands (doc 013 §6, "Commands must say when"): a scheduled
//! event or parameter ramp lands on its exact frame — across ragged blocks and
//! across a block boundary — in the executor and the reference alike; late is
//! counted and never dropped; the queue back-pressures instead of growing.

mod common;

use std::sync::{Arc, Mutex};

use common::prepare;
use tutti_graph::{
    Cx, Editor, EventIn, EventKind, Executor, Io, Node, ParamRamp, Prepare, Reference,
    ScheduleError, Shape, Status, Transport, Ump, COMMAND_CAPACITY,
};
use tutti_types::graph::{OutPort, Source};
use tutti_types::{At, Beat, Bpm, ChannelLayout, Frame, NodeKey, Samples, Tail};

/// `(absolute frame, block start, offset, tag)` per event received.
type Log = Arc<Mutex<Vec<(u64, u64, u32, u32)>>>;

const PROBE: NodeKey = NodeKey(7);
const MAX: usize = 64;

/// One event input, one audio output. Written against `Io::sub_blocks`: each
/// chunk applies its events, then renders its range — so a ramp's target
/// (a step: every ramp here has zero duration) shows on exactly its frame.
struct Probe {
    log: Log,
    level: f32,
}

impl Node for Probe {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
            .with_events(1, 0)
            .with_tail(Tail::Unknown)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        for (range, events) in io.sub_blocks(0) {
            for e in events {
                let tag = match e.kind {
                    EventKind::Midi(Ump(w)) => w[0],
                    EventKind::Ramp(r) => {
                        self.level = r.foreign_target(0).expect("ramps address id 0");
                        u32::MAX
                    }
                };
                self.log.lock().unwrap().push((
                    cx.env.frame_at(e.offset).get(),
                    cx.env.frame.get(),
                    e.offset.get(),
                    tag,
                ));
            }
            io.output(0)[range].fill(self.level);
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

fn tag(t: u32) -> EventKind {
    EventKind::Midi(Ump([t, 0, 0, 0]))
}

fn to() -> EventIn {
    EventIn {
        node: PROBE,
        port: 0,
    }
}

/// An editor and executor running one `Probe`, plus a reference running
/// another, over the same spec.
struct Rig {
    ed: Editor,
    exec: Executor,
    reference: Reference,
    exec_log: Log,
    ref_log: Log,
}

fn rig() -> Rig {
    let (exec_log, ref_log) = (Log::default(), Log::default());
    let (mut ed, mut exec) = Editor::new(prepare(MAX));
    ed.insert(
        PROBE,
        "probe",
        Probe {
            log: Arc::clone(&exec_log),
            level: 0.0,
        },
    );
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort {
        node: PROBE,
        port: 0,
    })];
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    let mut reference = Reference::new(prepare(MAX));
    reference.set_graph(
        &ed.spec().validate().unwrap(),
        [(
            PROBE,
            Box::new(Probe {
                log: Arc::clone(&ref_log),
                level: 0.0,
            }) as Box<dyn Node>,
        )]
        .into(),
    );
    Rig {
        ed,
        exec,
        reference,
        exec_log,
        ref_log,
    }
}

impl Rig {
    fn schedule(&mut self, at: At, kind: EventKind) {
        self.ed.schedule(at, to(), kind).expect("room");
        self.reference.schedule(at, to(), kind);
    }

    /// Render `n` frames through both; return both outputs.
    fn block(&mut self, n: usize, transport: &Transport) -> (Vec<f32>, Vec<f32>) {
        let (mut a, mut b) = (vec![0.0; n], vec![0.0; n]);
        self.exec.process(n, transport, &[], &mut [&mut a[..]]);
        self.reference.process(n, transport, &[], &mut [&mut b[..]]);
        (a, b)
    }
}

/// A command at frame `f` lands on exactly offset `f - block_start`, under a
/// ragged block schedule — at a block's first frame, its last, in the middle,
/// and on both sides of a boundary — in both interpreters.
///
/// Mutation: in `Env::due`, compute the offset from the block's *end*
/// instead of its start (`f.since(self.end())`-style off-by-a-block) →
/// nothing lands where it should → fails. Mutation: in `CommandRx::gather`,
/// deliver every command at `Offset::ZERO` → fails.
#[test]
fn a_command_lands_on_its_exact_frame_across_ragged_blocks() {
    let mut rig = rig();
    let blocks = [64usize, 7, 1, 33, 64, 5, 64, 2];
    let starts: Vec<u64> = blocks
        .iter()
        .scan(0u64, |at, &n| {
            let s = *at;
            *at += n as u64;
            Some(s)
        })
        .collect();
    // Block starts (64, 71, 72, 105, …), last frames (63, 70, 104), the
    // middle of a block, and the two frames either side of a boundary.
    let targets: Vec<u64> = vec![
        0, 5, 63, 64, 70, 71, 72, 104, 105, 150, 168, 169, 173, 174, 238,
    ];
    for (i, &f) in targets.iter().enumerate() {
        rig.schedule(At::Frame(Frame(f)), tag(i as u32));
    }
    let transport = Transport::default();
    for &n in &blocks {
        rig.block(n, &transport);
    }
    for log in [&rig.exec_log, &rig.ref_log] {
        let got = log.lock().unwrap().clone();
        let want: Vec<(u64, u64, u32, u32)> = targets
            .iter()
            .enumerate()
            .map(|(i, &f)| {
                let start = *starts.iter().rev().find(|&&s| s <= f).unwrap();
                (f, start, (f - start) as u32, i as u32)
            })
            .collect();
        assert_eq!(got, want);
    }
    assert_eq!(rig.exec.late_commands(), 0);
    assert_eq!(rig.ed.commands_outstanding(), 0, "every credit came back");
}

/// A scheduled ramp changes the output on exactly its frame: the node reads
/// it through `sub_blocks`, so the step is at the sample, not the block.
///
/// Mutation: in `Probe`, fill the whole block after applying all events
/// (ignore `range`) → the step moves to the block start → fails. Mutation:
/// in the executor, skip the overlay for a node with scheduled commands
/// (`scheduled` always false) → the ramp never arrives → fails.
#[test]
fn a_scheduled_ramp_steps_the_output_on_its_frame() {
    let mut rig = rig();
    rig.schedule(
        At::Frame(Frame(90)),
        EventKind::Ramp(ParamRamp::foreign(0, 0.5, Samples(0))),
    );
    let (mut a, mut b) = (Vec::new(), Vec::new());
    for n in [64, 64] {
        let (x, y) = rig.block(n, &Transport::default());
        a.extend(x);
        b.extend(y);
    }
    let want: Vec<f32> = (0..128).map(|i| if i < 90 { 0.0 } else { 0.5 }).collect();
    assert_eq!(a, want);
    assert_eq!(b, want);
}

/// A time already past lands at offset 0 of the next block and is counted
/// late — not dropped. `NextBlock` lands at offset 0 and is not late.
///
/// Mutation: in `CommandRx::gather`, keep a late command pending (treat
/// `Due::Late` like `NotYet`) → it never lands → fails. Mutation: drop the
/// `late += 1` → the count stays 0 → fails.
#[test]
fn a_late_command_lands_at_the_next_block_and_is_counted() {
    let mut rig = rig();
    let t = Transport::default();
    rig.block(64, &t);
    rig.block(64, &t);
    rig.schedule(At::Frame(Frame(10)), tag(1)); // 118 frames ago
    rig.schedule(At::NextBlock, tag(2));
    rig.block(32, &t);
    for log in [&rig.exec_log, &rig.ref_log] {
        assert_eq!(
            log.lock().unwrap().clone(),
            vec![(128, 128, 0, 1), (128, 128, 0, 2)],
            "both at offset 0 of the block starting at 128, in scheduling order"
        );
    }
    assert_eq!(rig.exec.late_commands(), 1);
    assert_eq!(rig.reference.late_commands(), 1);
}

/// `At::Beat` resolves against the transport snapshot of the block where it
/// falls: 120 BPM at 48 kHz is 24 000 frames a beat, so beat 1.5 is frame
/// 36 000 — here inside a block that starts at 35 990.
///
/// Mutation: resolve beats against the transport of the block the command
/// was *first seen* in (cache the frame then) with a tempo change between →
/// lands on the old tempo's frame → fails.
#[test]
fn a_beat_command_lands_where_the_transport_reaches_it() {
    let mut rig = rig();
    rig.schedule(At::Beat(Beat(1.5)), tag(3));
    let rate = 48_000.0;
    let mut frame = 0u64;
    // First blocks at 60 BPM (48 000 frames a beat), then 120: the beat is
    // reached where the *current* tempo puts it.
    let mut beat = 0.0f64;
    for n in [64usize; 4] {
        let t = Transport {
            playing: true,
            tempo: Bpm(60.0),
            beat: Beat(beat),
            looping: None,
        };
        rig.block(n, &t);
        beat += n as f64 * 60.0 / (60.0 * rate); // 60 BPM: beats per frame
        frame += n as u64;
    }
    // Jump to just before beat 1.5 at 120 BPM: 10 frames short.
    let spb = 120.0 / (60.0 * rate); // beats per frame
    let t = Transport {
        playing: true,
        tempo: Bpm(120.0),
        beat: Beat(1.5 - 10.0 * spb),
        looping: None,
    };
    rig.block(64, &t);
    for log in [&rig.exec_log, &rig.ref_log] {
        assert_eq!(
            log.lock().unwrap().clone(),
            vec![(frame + 10, frame, 10, 3)]
        );
    }
    assert_eq!(rig.exec.late_commands(), 0);
}

/// Back-pressure: at `COMMAND_CAPACITY` outstanding, `schedule` refuses and
/// sends nothing; once commands land their credit returns.
///
/// Mutation: in `CommandTx::send`, drop the credit check → the ring's push
/// fails and hits the `unreachable!` → fails.
#[test]
fn scheduling_back_pressures_at_capacity() {
    let mut rig = rig();
    for i in 0..COMMAND_CAPACITY {
        rig.ed
            .schedule(At::Frame(Frame(100 + i as u64)), to(), tag(i as u32))
            .expect("under capacity");
    }
    assert_eq!(
        rig.ed.schedule(At::NextBlock, to(), tag(0)),
        Err(ScheduleError::Backpressure)
    );
    // Frames 100..=163 land in the second 64-block (64..128) only up to 127.
    let t = Transport::default();
    rig.exec.process(64, &t, &[], &mut [&mut [0.0; 64][..]]);
    rig.exec.process(64, &t, &[], &mut [&mut [0.0; 64][..]]);
    assert_eq!(rig.ed.commands_outstanding(), COMMAND_CAPACITY - 28);
    assert!(rig.ed.schedule(At::NextBlock, to(), tag(0)).is_ok());
    assert_eq!(rig.exec.late_commands(), 0);
}

/// A command must name a port the plan has; one whose node is removed —
/// or replaced by a unit without that port — before it falls due is counted
/// unrouted, not delivered somewhere else.
///
/// Mutation: in `CommandRx::gather`, skip the port-range filter → the
/// command to the replaced node's vanished port is "delivered", and the
/// executor panics indexing the node's event inputs → fails.
#[test]
fn a_command_needs_a_port_and_an_orphan_is_counted() {
    let mut rig = rig();
    assert_eq!(
        rig.ed.schedule(
            At::NextBlock,
            EventIn {
                node: PROBE,
                port: 1
            },
            tag(0)
        ),
        Err(ScheduleError::NoSuchPort {
            to: EventIn {
                node: PROBE,
                port: 1
            }
        })
    );
    rig.ed
        .schedule(At::Frame(Frame(1000)), to(), tag(9))
        .expect("the port exists now");
    rig.ed
        .schedule(At::Frame(Frame(3000)), to(), tag(10))
        .expect("the port exists now");
    // Same key, a unit with no event inputs at all.
    rig.ed.insert(
        PROBE,
        "const",
        common::TestNode::new(common::Kind::Const {
            value: 1.0,
            width: 1,
        }),
    );
    rig.ed.commit().expect("commits");
    let t = Transport::default();
    for _ in 0..20 {
        rig.exec.process(64, &t, &[], &mut [&mut [0.0; 64][..]]);
    }
    assert_eq!(
        rig.exec.unrouted_commands(),
        1,
        "the port went with the unit"
    );
    rig.ed
        .schedule(At::Frame(Frame(2000)), to(), tag(9))
        .expect_err("and the editor knows it");
    rig.ed.remove(PROBE);
    rig.ed.spec_mut().topology.outputs.clear();
    rig.ed.commit().expect("commits");
    for _ in 0..40 {
        rig.exec.process(64, &t, &[], &mut []);
    }
    assert_eq!(rig.exec.unrouted_commands(), 2, "and the key went entirely");
    assert_eq!(rig.ed.commands_outstanding(), 0);
    assert!(rig.exec_log.lock().unwrap().is_empty());
}

/// Before anything is committed there is nothing to address.
#[test]
fn scheduling_needs_a_plan() {
    let (mut ed, _exec) = Editor::new(prepare(MAX));
    assert_eq!(
        ed.schedule(At::NextBlock, to(), tag(0)),
        Err(ScheduleError::NoPlan)
    );
}

/// Passes its audio through and adds 1.0 on the frame of every event, which
/// it also logs as `(absolute frame, tag)`.
struct Impulse(Arc<Mutex<Vec<(u64, u32)>>>);

impl Node for Impulse {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO)
            .with_events(1, 0)
            .with_tail(Tail::Unknown)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.channel(0).map(|x| x);
        for e in io.events(0) {
            if let EventKind::Midi(Ump(w)) = e.kind {
                self.0
                    .lock()
                    .unwrap()
                    .push((cx.env.frame_at(e.offset).get(), w[0]));
            }
            io.output(0)[e.offset.index()] += 1.0;
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// PDC applies to scheduled commands as to upstream events: `At::Frame(F)`
/// is timeline frame `F`, which a sink behind a 20-frame latent path hears at
/// its own frame `F + 20`. So the note lands 20 frames later there than at a
/// sibling with arrival 0 — and both impulses come out of the graph on the
/// same output frame, the sibling's channel delayed by the output alignment.
///
/// Mutation: drop the arrival in `CommandRx::gather` (`Latency::ZERO`) →
/// the latent sink's note lands at 100, 20 frames early against its own
/// audio, and its impulse leaves 20 frames before the sibling's → fails.
/// Mutation: the same in the reference only → fails on the reference.
#[test]
fn a_scheduled_command_is_compensated_like_an_upstream_event() {
    const LAT: NodeKey = NodeKey(1);
    const A: NodeKey = NodeKey(2);
    const B: NodeKey = NodeKey(3);
    let units = |log: &Arc<Mutex<Vec<(u64, u32)>>>| -> Vec<(NodeKey, Box<dyn Node>)> {
        vec![
            (
                LAT,
                Box::new(common::TestNode::new(common::Kind::Lag { latency: 20 })),
            ),
            (A, Box::new(Impulse(Arc::clone(log)))),
            (B, Box::new(Impulse(Arc::clone(log)))),
        ]
    };
    let (exec_log, ref_log) = (Arc::default(), Arc::default());
    let (mut ed, mut exec) = Editor::new(prepare(MAX));
    for (k, u) in units(&exec_log) {
        ed.insert(k, "n", u);
    }
    let t = &mut ed.spec_mut().topology;
    t.inputs = ChannelLayout::MONO;
    t.edges.insert(
        tutti_types::graph::InPort { node: LAT, port: 0 },
        tutti_types::graph::Edge::Direct(Source::Global(0)),
    );
    t.edges.insert(
        tutti_types::graph::InPort { node: A, port: 0 },
        tutti_types::graph::Edge::Direct(Source::Node(OutPort { node: LAT, port: 0 })),
    );
    t.edges.insert(
        tutti_types::graph::InPort { node: B, port: 0 },
        tutti_types::graph::Edge::Direct(Source::Global(0)),
    );
    t.outputs = vec![
        Source::Node(OutPort { node: A, port: 0 }),
        Source::Node(OutPort { node: B, port: 0 }),
    ];
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    assert_eq!(
        exec.plan().unwrap().unit(A).unwrap().arrival.samples(),
        Samples(20)
    );
    let mut reference = Reference::new(prepare(MAX));
    reference.set_graph(
        &ed.spec().validate().unwrap(),
        units(&ref_log).into_iter().collect(),
    );
    for (k, t) in [(A, 1), (B, 2)] {
        let to = EventIn { node: k, port: 0 };
        ed.schedule(At::Frame(Frame(100)), to, tag(t))
            .expect("room");
        reference.schedule(At::Frame(Frame(100)), to, tag(t));
    }
    let silence = [0.0f32; 64];
    let (mut outs, mut routs) = (vec![Vec::new(); 2], vec![Vec::new(); 2]);
    for n in [64usize, 7, 64, 33] {
        let (mut a, mut b) = (vec![0.0; n], vec![0.0; n]);
        exec.process(
            n,
            &Transport::default(),
            &[&silence[..n]],
            &mut [&mut a[..], &mut b[..]],
        );
        outs[0].extend(a);
        outs[1].extend(b);
        let (mut a, mut b) = (vec![0.0; n], vec![0.0; n]);
        reference.process(
            n,
            &Transport::default(),
            &[&silence[..n]],
            &mut [&mut a[..], &mut b[..]],
        );
        routs[0].extend(a);
        routs[1].extend(b);
    }
    for (log, out) in [(&exec_log, &outs), (&ref_log, &routs)] {
        let mut got = log.lock().unwrap().clone();
        got.sort();
        assert_eq!(
            got,
            vec![(100, 2), (120, 1)],
            "the latent sink hears it 20 later"
        );
        for ch in out.iter() {
            let hits: Vec<usize> = ch
                .iter()
                .enumerate()
                .filter(|(_, &x)| x != 0.0)
                .map(|(i, _)| i)
                .collect();
            assert_eq!(hits, vec![120], "both impulses leave on the same frame");
        }
    }
    assert_eq!(exec.late_commands(), 0);
}

/// The hold, end to end with the editor on its own thread: every round
/// inserts a fresh node, commits it and at once schedules a `NextBlock` note
/// to it, while the audio thread spins one-frame blocks. The invariant, over
/// every round: every note is delivered exactly once, none unrouted.
///
/// Honest about its reach: the race the hold guards (the executor pulling
/// the command before the commit) needs the editor's two pushes to fall in
/// the few instructions between the executor's commit pass and its command
/// pass, and with the hold removed this test did not catch it in 15 000
/// rounds here. The deterministic staged version is the unit test
/// `exec::tests::a_command_seen_before_its_commit_waits_for_it`, which does.
/// What this one does catch: anything that loses, repeats or strands a
/// command across threads. Mutation: skip the `done.fetch_add` in
/// `CommandRx::gather` → the editor never sees its credit back and the
/// round waits out its deadline → fails.
#[test]
fn a_command_waits_for_the_commit_it_was_checked_against() {
    use std::sync::atomic::{AtomicBool, Ordering};
    const ROUNDS: u32 = 3_000;
    let log: Log = Arc::default();
    let (mut ed, mut exec) = Editor::new(prepare(MAX));
    ed.insert(
        NodeKey(0),
        "probe",
        Probe {
            log: Arc::clone(&log),
            level: 0.0,
        },
    );
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    let done = Arc::new(AtomicBool::new(false));
    let stop = Arc::clone(&done);
    let audio = std::thread::spawn(move || {
        let t = Transport::default();
        while !stop.load(Ordering::Acquire) {
            exec.process(1, &t, &[], &mut []);
        }
        exec
    });
    for round in 1..=ROUNDS {
        let key = NodeKey(u64::from(round));
        ed.insert(
            key,
            "probe",
            Probe {
                log: Arc::clone(&log),
                level: 0.0,
            },
        );
        ed.remove(NodeKey(u64::from(round - 1)));
        loop {
            match ed.commit() {
                Ok(()) => break,
                Err(tutti_graph::CommitError::Backpressure) => std::thread::yield_now(),
                Err(e) => panic!("{e}"),
            }
        }
        ed.schedule(At::NextBlock, EventIn { node: key, port: 0 }, tag(round))
            .expect("one outstanding at a time");
        // Wait for it to land before the next round removes its node.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while ed.commands_outstanding() > 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "round {round}: the command never landed"
            );
            std::thread::yield_now();
        }
    }
    done.store(true, Ordering::Release);
    let exec = audio.join().expect("the audio thread");
    ed.collect();
    assert_eq!(
        exec.unrouted_commands(),
        0,
        "a command was judged against a stale plan"
    );
    let mut tags: Vec<u32> = log.lock().unwrap().iter().map(|e| e.3).collect();
    tags.sort_unstable();
    assert_eq!(tags, (1..=ROUNDS).collect::<Vec<_>>());
}

/// `cancel` and `cancel_all` take commands back and return their credit —
/// even with every credit held by commands that will never fall due (beats
/// on a stopped transport), where nothing else could be scheduled.
///
/// Mutation: in `CommandRx::pull`, do not return the credit of what it
/// cancels (drop the `done.fetch_add`) → `schedule` still refuses after the
/// `cancel_all` → fails. Mutation: apply cancels before pulling new
/// commands → the targeted cancel misses its command, which lands → fails.
#[test]
fn cancel_takes_commands_back_and_frees_their_credit() {
    let mut rig = rig();
    let stopped = Transport::default();
    // A targeted cancel, sent before the executor has seen its command.
    let id = rig
        .ed
        .schedule(At::Frame(Frame(10)), to(), tag(7))
        .expect("room");
    rig.ed.cancel(id).expect("room");
    rig.block(64, &stopped);
    assert!(
        rig.exec_log.lock().unwrap().is_empty(),
        "cancelled, not landed"
    );
    assert_eq!(rig.exec.cancelled_commands(), 1);
    // Cancelling one that already landed is a no-op.
    let landed = rig.ed.schedule(At::NextBlock, to(), tag(8)).expect("room");
    rig.block(64, &stopped);
    rig.ed.cancel(landed).expect("room");
    rig.block(64, &stopped);
    assert_eq!(rig.exec.cancelled_commands(), 1);

    for i in 0..COMMAND_CAPACITY as u32 {
        rig.ed
            .schedule(At::Beat(Beat(1.0 + f64::from(i))), to(), tag(i))
            .expect("under capacity");
    }
    rig.block(64, &stopped);
    assert_eq!(
        rig.ed.schedule(At::NextBlock, to(), tag(0)),
        Err(ScheduleError::Backpressure),
        "every credit is held by a beat the stopped transport never reaches"
    );
    rig.ed
        .cancel_all()
        .expect("the cancel ring needs no credit");
    rig.block(64, &stopped);
    assert_eq!(rig.ed.commands_outstanding(), 0);
    assert_eq!(rig.exec.cancelled_commands(), 1 + COMMAND_CAPACITY as u64);
    assert!(rig.ed.schedule(At::NextBlock, to(), tag(0)).is_ok());
}

/// A ramp into a node that does not honour offsets sample-accurately is
/// refused at `schedule`, as a marked edge would be at `compile`.
///
/// Mutation: drop the resolution check in `Editor::schedule` → accepted →
/// fails.
#[test]
fn a_ramp_into_a_coarse_node_is_refused() {
    struct Coarse;
    impl Node for Coarse {
        fn shape(&self) -> Shape {
            Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY)
                .with_events(1, 0)
                .with_event_resolution(tutti_graph::Resolution::Frames(8))
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
            Status::Silent
        }
        fn reset(&mut self) {}
    }
    let (mut ed, mut exec) = Editor::new(prepare(MAX));
    ed.insert(PROBE, "coarse", Coarse);
    ed.commit().expect("commits");
    exec.apply_pending();
    let ramp = EventKind::Ramp(ParamRamp::foreign(0, 1.0, Samples(0)));
    assert_eq!(
        ed.schedule(At::NextBlock, to(), ramp),
        Err(ScheduleError::ResolutionTooCoarse {
            to: to(),
            sink: tutti_graph::Resolution::Frames(8)
        })
    );
    assert!(
        ed.schedule(At::NextBlock, to(), tag(1)).is_ok(),
        "a note is fine"
    );
}

/// A transport at `beat`. The oracle table runs at 1440 BPM and 48 kHz:
/// 2 000 frames a beat, so a 500-frame block is a quarter beat.
fn at_beat(beat: f64, playing: bool, tempo: f64, looping: Option<(f64, f64)>) -> Transport {
    Transport {
        playing,
        tempo: Bpm(tempo),
        beat: Beat(beat),
        looping: looping.map(|(start, end)| tutti_graph::LoopRange {
            start: Beat(start),
            end: Beat(end),
        }),
    }
}

/// One case of the oracle table: blocks of 500 frames, each with its
/// transport (hand-computed), a beat command scheduled before block
/// `sched_at`, into a sink behind `arrival` frames of latency; where it must
/// land (absolute frame at the sink) and whether late — or `None`, never.
struct Case {
    name: &'static str,
    arrival: usize,
    transports: Vec<Transport>,
    sched_at: usize,
    beat: f64,
    want: Option<(u64, bool)>,
}

/// Beat 0.5 to 1.75 in quarter-beat blocks, a wrap of loop [1, 2) at frame
/// 3 000, then 1.0 to 1.75 again.
fn looped_from_half() -> Vec<Transport> {
    [0.5, 0.75, 1.0, 1.25, 1.5, 1.75, 1.0, 1.25, 1.5, 1.75]
        .into_iter()
        .map(|b| at_beat(b, true, 1440.0, Some((1.0, 2.0))))
        .collect()
}

fn continuous(from: f64, tempo: f64, blocks: usize) -> Vec<Transport> {
    (0..blocks)
        .map(|i| {
            at_beat(
                from + i as f64 * 500.0 * tempo / 60.0 / 48_000.0,
                true,
                tempo,
                None,
            )
        })
        .collect()
}

/// Beat → frame resolution, checked against a table worked out by hand —
/// independent of both interpreters, which share `Env::due_at_arrival` and
/// the `Playhead` (so a bug there would pass the differential). Covers tempo,
/// a loop wrap, a stopped transport, a seek over the beat and back, a beat
/// crossed before it was scheduled (late), and an arrival shift.
///
/// Mutation: in `Playhead::crossed`, treat every beat behind the playhead as
/// crossed → the seek-over case lands late at the seek → fails. Mutation:
/// after a wrap, call the whole loop crossed (`beat < loop end` instead of
/// `beat < now`, the reviewed bug) → the beat ahead of the playhead fires
/// late at once → fails. Mutation:
/// in `Env::beat_due`, drop the wrap branch → the loop case never lands →
/// fails. Mutation: in `Env::due_at_arrival`, drop the arrival shift for a
/// resolved beat → the arrival case lands 20 frames early → fails.
#[test]
fn beat_resolution_matches_a_hand_computed_table() {
    let cases = vec![
        Case {
            name: "plain: beat 1.25 is frame 2 500",
            arrival: 0,
            transports: continuous(0.0, 1440.0, 8),
            sched_at: 0,
            beat: 1.25,
            want: Some((2_500, false)),
        },
        Case {
            name: "behind a 20-frame latent path: 2 520",
            arrival: 20,
            transports: continuous(0.0, 1440.0, 8),
            sched_at: 0,
            beat: 1.25,
            want: Some((2_520, false)),
        },
        Case {
            // 720 BPM for two blocks (0.125 beat each), then 1440 from beat
            // 0.25 at frame 1 000: beat 0.6 is 0.35 beat = 700 frames later.
            name: "tempo change",
            arrival: 0,
            transports: vec![
                at_beat(0.0, true, 720.0, None),
                at_beat(0.125, true, 720.0, None),
                at_beat(0.25, true, 1440.0, None),
                at_beat(0.5, true, 1440.0, None),
                at_beat(0.75, true, 1440.0, None),
            ],
            sched_at: 0,
            beat: 0.6,
            want: Some((1_700, false)),
        },
        Case {
            // Loop [1, 2) from beat 1.8: the wrap is 0.2 beat = 400 frames in;
            // beat 1.02 is 40 frames after it.
            name: "loop wrap",
            arrival: 0,
            transports: vec![
                at_beat(1.8, true, 1440.0, Some((1.0, 2.0))),
                at_beat(1.05, true, 1440.0, Some((1.0, 2.0))),
            ],
            sched_at: 0,
            beat: 1.02,
            want: Some((440, false)),
        },
        Case {
            // Stopped at 0 for three blocks, then rolling from frame 1 500.
            name: "stopped, then playing",
            arrival: 0,
            transports: vec![
                at_beat(0.0, false, 1440.0, None),
                at_beat(0.0, false, 1440.0, None),
                at_beat(0.0, false, 1440.0, None),
                at_beat(0.0, true, 1440.0, None),
            ],
            sched_at: 0,
            beat: 0.1,
            want: Some((1_700, false)),
        },
        Case {
            // Rolling from 0; at frame 1 000 a seek to beat 3 jumps over 2.0
            // (not fired); at frame 2 000 a seek back to 1.9 reaches it 0.1
            // beat = 200 frames later.
            name: "seek over, then back",
            arrival: 0,
            transports: vec![
                at_beat(0.0, true, 1440.0, None),
                at_beat(0.25, true, 1440.0, None),
                at_beat(3.0, true, 1440.0, None),
                at_beat(3.25, true, 1440.0, None),
                at_beat(1.9, true, 1440.0, None),
                at_beat(2.15, true, 1440.0, None),
            ],
            sched_at: 0,
            beat: 2.0,
            want: Some((2_200, false)),
        },
        Case {
            // Scheduled before block 2 (frame 1 000, beat 0.5), when playback
            // has already crossed 0.1: late, at offset 0 of that block.
            name: "crossed before it was scheduled",
            arrival: 0,
            transports: continuous(0.0, 1440.0, 4),
            sched_at: 2,
            beat: 0.1,
            want: Some((1_000, true)),
        },
        Case {
            // Loop [1, 2) entered from beat 0.5: through the loop, wrap at
            // frame 3 000, scheduled before block 7 (frame 3 500, beat 1.25).
            // Beat 1.6 is ahead of the playhead in this pass — the last pass
            // crossed it, this one reaches it 0.35 beat = 700 frames on.
            name: "after a wrap: ahead of the playhead waits",
            arrival: 0,
            transports: looped_from_half(),
            sched_at: 7,
            beat: 1.6,
            want: Some((4_200, false)),
        },
        Case {
            name: "after a wrap: behind the playhead in this pass is late",
            arrival: 0,
            transports: looped_from_half(),
            sched_at: 7,
            beat: 1.1,
            want: Some((3_500, true)),
        },
        Case {
            name: "after a wrap: before the loop (crossed on the way in) is late",
            arrival: 0,
            transports: looped_from_half(),
            sched_at: 7,
            beat: 0.6,
            want: Some((3_500, true)),
        },
        Case {
            // Looping [1, 2) forever: beat 2.5 is never reached.
            name: "past the loop end",
            arrival: 0,
            transports: (0..8)
                .map(|i| {
                    let b = 1.0 + (i as f64 * 0.25) % 1.0;
                    at_beat(b, true, 1440.0, Some((1.0, 2.0)))
                })
                .collect(),
            sched_at: 0,
            beat: 2.5,
            want: None,
        },
    ];
    for case in cases {
        let log: Arc<Mutex<Vec<(u64, u32)>>> = Arc::default();
        let ref_log: Arc<Mutex<Vec<(u64, u32)>>> = Arc::default();
        let (lat, sink) = (NodeKey(1), NodeKey(2));
        let (mut ed, mut exec) = Editor::new(prepare(500));
        let mut reference = Reference::new(prepare(500));
        let build = |ed: &mut Editor, log: &Arc<Mutex<Vec<(u64, u32)>>>| {
            ed.insert(
                lat,
                "lag",
                common::TestNode::new(common::Kind::Lag {
                    latency: case.arrival,
                }),
            );
            ed.insert(sink, "impulse", Impulse(Arc::clone(log)));
        };
        build(&mut ed, &log);
        let t = &mut ed.spec_mut().topology;
        t.inputs = ChannelLayout::MONO;
        t.edges.insert(
            tutti_types::graph::InPort { node: lat, port: 0 },
            tutti_types::graph::Edge::Direct(Source::Global(0)),
        );
        t.edges.insert(
            tutti_types::graph::InPort {
                node: sink,
                port: 0,
            },
            tutti_types::graph::Edge::Direct(Source::Node(OutPort { node: lat, port: 0 })),
        );
        ed.commit().expect("commits");
        exec.apply_pending();
        ed.collect();
        let mut units: std::collections::BTreeMap<NodeKey, Box<dyn Node>> = Default::default();
        units.insert(
            lat,
            Box::new(common::TestNode::new(common::Kind::Lag {
                latency: case.arrival,
            })),
        );
        units.insert(sink, Box::new(Impulse(Arc::clone(&ref_log))));
        reference.set_graph(&ed.spec().validate().unwrap(), units);
        let to = EventIn {
            node: sink,
            port: 0,
        };
        let silence = [0.0f32; 500];
        for (i, tr) in case.transports.iter().enumerate() {
            if i == case.sched_at {
                ed.schedule(At::Beat(Beat(case.beat)), to, tag(9))
                    .expect("room");
                reference.schedule(At::Beat(Beat(case.beat)), to, tag(9));
            }
            exec.process(500, tr, &[&silence[..]], &mut []);
            reference.process(500, tr, &[&silence[..]], &mut []);
        }
        let want: Vec<(u64, u32)> = case.want.iter().map(|&(f, _)| (f, 9)).collect();
        assert_eq!(*log.lock().unwrap(), want, "executor: {}", case.name);
        assert_eq!(*ref_log.lock().unwrap(), want, "reference: {}", case.name);
        let late = u64::from(case.want.is_some_and(|w| w.1));
        assert_eq!(exec.late_commands(), late, "{}", case.name);
        assert_eq!(reference.late_commands(), late, "{}", case.name);
    }
}

/// A command whose port vanished (its node replaced by one without event
/// inputs) is still timed by the node's arrival: both interpreters count it
/// unrouted when timeline frame 100 reaches the node, at its own frame 120 —
/// not at 100.
///
/// Mutation: in `CommandRx::gather`, take the arrival from the port-filtered
/// target (0 when the port is gone) → the executor counts it at 100, before
/// the reference → fails.
#[test]
fn an_unroutable_command_is_timed_by_its_nodes_arrival() {
    let (lat, sink) = (NodeKey(1), NodeKey(2));
    let log: Arc<Mutex<Vec<(u64, u32)>>> = Arc::default();
    let (mut ed, mut exec) = Editor::new(prepare(MAX));
    ed.insert(
        lat,
        "lag",
        common::TestNode::new(common::Kind::Lag { latency: 20 }),
    );
    ed.insert(sink, "impulse", Impulse(Arc::clone(&log)));
    let t = &mut ed.spec_mut().topology;
    t.inputs = ChannelLayout::MONO;
    t.edges.insert(
        tutti_types::graph::InPort { node: lat, port: 0 },
        tutti_types::graph::Edge::Direct(Source::Global(0)),
    );
    t.edges.insert(
        tutti_types::graph::InPort {
            node: sink,
            port: 0,
        },
        tutti_types::graph::Edge::Direct(Source::Node(OutPort { node: lat, port: 0 })),
    );
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    let mut reference = Reference::new(prepare(MAX));
    let mut units: std::collections::BTreeMap<NodeKey, Box<dyn Node>> = Default::default();
    units.insert(
        lat,
        Box::new(common::TestNode::new(common::Kind::Lag { latency: 20 })),
    );
    units.insert(sink, Box::new(Impulse(Arc::default())));
    reference.set_graph(&ed.spec().validate().unwrap(), units);

    let to = EventIn {
        node: sink,
        port: 0,
    };
    ed.schedule(At::Frame(Frame(100)), to, tag(1))
        .expect("room");
    reference.schedule(At::Frame(Frame(100)), to, tag(1));
    // Same key, a unit with no event inputs.
    let gain = || {
        common::TestNode::new(common::Kind::Gain {
            gain: 1.0,
            width: 1,
        })
    };
    ed.insert(sink, "gain", gain());
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    let mut units: std::collections::BTreeMap<NodeKey, Box<dyn Node>> = Default::default();
    units.insert(sink, Box::new(gain()));
    reference.set_graph(&ed.spec().validate().unwrap(), units);

    let silence = [0.0f32; 10];
    for block in 0..14 {
        exec.process(10, &Transport::default(), &[&silence[..]], &mut []);
        reference.process(10, &Transport::default(), &[&silence[..]], &mut []);
        let want = u64::from(block >= 12); // the block holding frame 120
        assert_eq!(exec.unrouted_commands(), want, "executor, block {block}");
        assert_eq!(
            reference.unrouted_commands(),
            want,
            "reference, block {block}"
        );
    }
}

/// Cancels are never lost with the editor on its own thread: every round
/// schedules a command far in the future and cancels it at once, while the
/// audio thread spins one-frame blocks. A lost cancel would leave its command
/// pending forever, holding its credit. The staged, deterministic version of
/// the race is `command::tests::a_cancel_seen_before_its_command_is_held_until_it_arrives`.
///
/// Mutation: drop cancels that name a command not pulled yet (the pre-fix
/// behaviour) → in some runs a command outlives its cancel and the credit
/// never returns → the deadline fails. (The window is narrow: this is a
/// stress test, and the staged test is the one that always catches it.)
#[test]
fn cancels_are_never_lost_across_threads() {
    use std::sync::atomic::{AtomicBool, Ordering};
    const ROUNDS: u32 = 20_000;
    let rig = rig();
    let (mut ed, mut exec, log) = (rig.ed, rig.exec, rig.exec_log);
    let done = Arc::new(AtomicBool::new(false));
    let stop = Arc::clone(&done);
    let audio = std::thread::spawn(move || {
        let t = Transport::default();
        let mut out = [0.0f32; 1];
        while !stop.load(Ordering::Acquire) {
            exec.process(1, &t, &[], &mut [&mut out[..]]);
        }
        exec
    });
    for round in 0..ROUNDS {
        let id = loop {
            match ed.schedule(At::Frame(Frame(u64::MAX / 2)), to(), tag(round)) {
                Ok(id) => break id,
                Err(ScheduleError::Backpressure) => std::thread::yield_now(),
                Err(e) => panic!("{e}"),
            }
        };
        while ed.cancel(id) == Err(ScheduleError::Backpressure) {
            std::thread::yield_now();
        }
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while ed.commands_outstanding() > 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "{} commands outlived their cancels",
            ed.commands_outstanding()
        );
        std::thread::yield_now();
    }
    done.store(true, Ordering::Release);
    let exec = audio.join().expect("the audio thread");
    assert_eq!(exec.cancelled_commands(), u64::from(ROUNDS));
    assert!(log.lock().unwrap().is_empty());
}
