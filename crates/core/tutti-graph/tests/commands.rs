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
    assert_eq!(rig.ed.schedule(At::NextBlock, to(), tag(0)), Ok(()));
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
