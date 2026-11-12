//! Event taps as plan ops: one event output of any node copied out with the
//! frame each MIDI event was due on, whatever the block sizes were; aligned by
//! the compiler or raw; counted when the reader is slow, with a note-off kept
//! over any other event; reading nothing, and counting no loss, when the port
//! goes missing; a closed tap compiles to no op, a fork carries none;
//! bit-identical on the parallel executor and allocation-free on every
//! participant.
//!
//! Every figure is analytic (a script's frames, a declared latency, the ring's
//! capacity), none read from another render.
//!
//! Not covered here, by construction: a block with no reader writes nothing and
//! counts nothing. The count lives in an `Arc` only the reader reads, so once
//! the reader is dropped there is nobody to observe it; `NodeMode` (doc 013's
//! bypass and mute) is not in the graph yet, so a bypassed source's silence is
//! M1's to test.

use tutti_graph::testing as common;

use assert_no_alloc::AllocDisabler;
use common::{Kind, TestNode, prepare};
use tutti_fixtures::midi::{m1_note_off, m1_note_on};
use tutti_graph::{CloseError, EventTapId, EventTapReader};
use tutti_graph::{
    Cx, Edge, Editor, Event, EventOut, Executor, ForkByClone, ForkCause, ForkMode, ForkSource,
    ForkTarget, Forked, InPort, IntoNode, Io, MissingSource, Node, NodeKey, NodeParts, Offset, Op,
    OutPort, Prepare, Shape, Source, Status, TapAlign, TapEvent, Transport, note_off_room,
};
use tutti_types::{Frame, Latency, Samples};
use tutti_ump::{MidiChannel, NoteNumber, U7, Ump};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

const MAX_BLOCK: usize = 512;

/// A `Kind` whose fork is a fresh node of the same kind.
struct Forkable(Kind);

struct KindFork(Kind);

impl ForkSource for KindFork {
    fn fork(&self, _mode: ForkMode) -> Result<Forked, ForkCause> {
        Ok(Forked::new(Box::new(TestNode::new(self.0.clone()))))
    }
}

impl IntoNode for Forkable {
    type Controls = ();
    fn into_node(self) -> (Box<dyn Node>, ()) {
        (Box::new(TestNode::new(self.0)), ())
    }
    fn into_parts(self) -> NodeParts<()> {
        NodeParts {
            node: Box::new(TestNode::new(self.0.clone())),
            controls: (),
            fork: Some(Box::new(KindFork(self.0))),
        }
    }
}

/// MIDI 1.0, group 0, channel 0, note-on, velocity 100.
fn on(key: u32) -> Ump {
    m1_note_on(MidiChannel::FIRST, NoteNumber::of(key as u8), U7::of(100))
}

fn off(key: u32) -> Ump {
    m1_note_off(MidiChannel::FIRST, NoteNumber::of(key as u8), U7::of(0))
}

/// What a port of the script node carries: the script's packet with word 1
/// set to the port, so a reader can tell the ports apart.
fn on_port(ump: Ump, port: u16) -> Ump {
    Ump([ump.0[0], u32::from(port), 0, 0])
}

/// Plays a script, `(frame, packet)` in frame order, on each of `ports` event
/// outputs (the port number in word 1), each event on its absolute frame. With
/// `resync`, port 0 also carries a `Resync` at the start of every block: a
/// graph event that is not MIDI.
#[derive(Clone)]
struct Script {
    events: Vec<(u64, Ump)>,
    ports: u16,
    cap: u32,
    latency: usize,
    resync: bool,
}

impl Script {
    fn new(events: Vec<(u64, Ump)>) -> Self {
        Self {
            events,
            ports: 1,
            cap: 64,
            latency: 0,
            resync: false,
        }
    }
    fn ports(mut self, ports: u16) -> Self {
        self.ports = ports;
        self
    }
    fn latency(mut self, latency: usize) -> Self {
        self.latency = latency;
        self
    }
    fn resync(mut self) -> Self {
        self.resync = true;
        self
    }
}

impl Node for Script {
    fn shape(&self) -> Shape {
        Shape::EMPTY
            .with_events(0, self.ports)
            .with_event_capacity(self.cap)
            .with_latency(Latency::new(Samples(self.latency)))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        for p in 0..self.ports {
            let out = io.event_out(usize::from(p));
            if p == 0 && self.resync {
                let _ = out.push(Event::resync(Offset::ZERO));
            }
            for &(f, ump) in &self.events {
                if let Some(at) = cx.env.offset_of(Frame(f)) {
                    let _ = out.push(Event::midi(at, on_port(ump, p)));
                }
            }
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

fn out(node: u64) -> Source {
    Source::Node(OutPort {
        node: NodeKey(node),
        port: 0,
    })
}

fn port(node: u64, port: u16) -> EventOut {
    EventOut {
        node: NodeKey(node),
        port,
    }
}

struct Rig {
    ed: Editor,
    exec: Executor,
    gated: bool,
}

/// An editor with the latent audio path `Const -> Lag(m)` on the only output
/// (so the graph's output time is `m` frames).
fn rig(workers: usize, no_alloc: bool, m: usize) -> Rig {
    let (mut ed, mut exec) = Editor::new(prepare(MAX_BLOCK));
    if workers > 1 {
        exec.set_pool(Some(if no_alloc {
            common::TestPool::no_alloc(workers)
        } else {
            common::test_pool(workers)
        }));
    }
    ed.insert(
        NodeKey(90),
        "const",
        Forkable(Kind::Const {
            value: 1.0,
            width: 1,
        }),
    );
    ed.insert(NodeKey(91), "lag", Forkable(Kind::Lag { latency: m }));
    let t = &mut ed.spec_mut().topology;
    t.connect(
        InPort {
            node: NodeKey(91),
            port: 0,
        },
        Edge::Direct(out(90)),
    );
    t.set_outputs([out(91)]);
    Rig {
        ed,
        exec,
        gated: no_alloc,
    }
}

fn script(r: &mut Rig, key: u64, s: Script) {
    r.ed.insert(NodeKey(key), "script", ForkByClone(s));
}

fn commit(r: &mut Rig) {
    r.ed.commit().expect("compiles");
    r.exec.apply_pending();
    r.ed.collect();
}

/// Renders `total` frames in blocks of `sizes` taken in turn (the last one cut
/// to fit), starting at the executor's current frame.
fn render(r: &mut Rig, sizes: &[usize], total: usize) {
    let t = Transport::default();
    let (mut done, mut i) = (0, 0);
    while done < total {
        let n = sizes[i % sizes.len()].min(total - done);
        let mut chan = vec![0.0f32; n];
        {
            let mut outs: Vec<&mut [f32]> = vec![chan.as_mut_slice()];
            if r.gated {
                assert_no_alloc::assert_no_alloc(|| r.exec.process(n, &t, &[], &mut outs));
            } else {
                r.exec.process(n, &t, &[], &mut outs);
            }
        }
        r.ed.collect();
        done += n;
        i += 1;
    }
}

fn drain(tap: &mut EventTapReader) -> Vec<TapEvent> {
    let mut all = Vec::new();
    let mut buf = [TapEvent {
        frame: Frame(0),
        ump: Ump::default(),
    }; 64];
    loop {
        let n = tap.pop_into(&mut buf);
        if n == 0 {
            return all;
        }
        all.extend_from_slice(&buf[..n]);
    }
}

fn at(frame: u64, ump: Ump) -> TapEvent {
    TapEvent {
        frame: Frame(frame),
        ump,
    }
}

fn tap_ops(r: &Rig) -> usize {
    r.ed.base()
        .expect("a plan")
        .ops()
        .iter()
        .filter(|o| matches!(o, Op::EventTap { .. }))
        .count()
}

fn event_delays(r: &Rig) -> usize {
    r.ed.base()
        .expect("a plan")
        .ops()
        .iter()
        .filter(|o| matches!(o, Op::EventDelay { .. }))
        .count()
}

/// A script with events on, before and after block edges of every size below.
fn edges() -> Vec<(u64, Ump)> {
    [
        0u64, 1, 2, 2, 6, 7, 8, 63, 64, 65, 99, 100, 101, 511, 512, 513, 1000, 1023, 1024, 1999,
    ]
    .iter()
    .enumerate()
    .map(|(i, &f)| (f, on(i as u32)))
    .collect()
}

/// Every event lands at its absolute frame, whatever the blocks were: the tap
/// reads the script back, frame for frame and packet for packet, at block
/// sizes 1, 7, 64, 100 and 512, and at a mix (including events on a block's
/// first and last frame and two on one frame).
#[test]
fn every_event_lands_at_its_frame_across_block_sizes() {
    let want: Vec<TapEvent> = edges()
        .into_iter()
        .map(|(f, u)| at(f, on_port(u, 0)))
        .collect();
    for sizes in [
        &[1usize][..],
        &[7],
        &[64],
        &[100],
        &[512],
        &[1, 7, 64, 100, 512],
    ] {
        let mut r = rig(1, false, 0);
        script(&mut r, 1, Script::new(edges()));
        let mut tap =
            r.ed.open_event_tap(EventTapId(1), port(1, 0), TapAlign::Raw, 256)
                .expect("the port exists");
        assert_eq!(tap.rate(), common::RATE, "frames are on the graph's rate");
        commit(&mut r);
        render(&mut r, sizes, 2000);
        assert_eq!(drain(&mut tap), want, "block sizes {sizes:?}");
        assert_eq!(tap.take_lost(), 0);
    }
}

/// A reader that never drains: the ring takes `capacity` events of any kind,
/// then only note-offs up to `note_off_room(capacity)`, and every refusal is
/// counted once, in events. Eight note-ons then twenty-four of the packets
/// (twenty note-ons, twenty note-offs, in that order) at capacity 8: the first
/// 8 note-ons are held, 12 refused; the note-offs fill the reserve
/// (`note_off_room(8) = 17`, 9 of them held), 11 refused. 23 lost in all, and
/// the read clears the count.
#[test]
fn a_slow_reader_loses_events_counted_and_a_note_off_is_kept() {
    assert_eq!(note_off_room(8), 17);
    let ons = (0..20u32).map(|k| (u64::from(k), on(k)));
    let offs = (0..20u32).map(|k| (20 + u64::from(k), off(k)));
    let mut r = rig(1, false, 0);
    script(&mut r, 1, Script::new(ons.chain(offs).collect()));
    let mut tap =
        r.ed.open_event_tap(EventTapId(1), port(1, 0), TapAlign::Raw, 8)
            .expect("the port exists");
    commit(&mut r);
    render(&mut r, &[64], 64);
    assert_eq!(tap.available(), 17);
    let got = drain(&mut tap);
    let want: Vec<TapEvent> = (0..8u32)
        .map(|k| at(u64::from(k), on_port(on(k), 0)))
        .chain((0..9u32).map(|k| at(20 + u64::from(k), on_port(off(k), 0))))
        .collect();
    assert_eq!(
        got, want,
        "the first note-ons, then the note-offs the reserve held"
    );
    assert_eq!(tap.take_lost(), 12 + 11);
    assert_eq!(tap.take_lost(), 0, "the read clears the count");
}

/// A port that narrows or vanishes after the open reads nothing: the commit
/// succeeds, the plan has no op for the tap, nothing is counted lost, the
/// other tap plays on, and restoring the port resumes the stream (the writer
/// stays in the plan's table).
#[test]
fn a_narrowed_port_reads_nothing_and_the_stream_resumes() {
    let mut r = rig(1, false, 0);
    let s = Script::new(vec![(10, on(1)), (150, on(2)), (250, on(3))]).ports(2);
    script(&mut r, 1, s.clone());
    let mut wide =
        r.ed.open_event_tap(EventTapId(1), port(1, 1), TapAlign::Raw, 64)
            .expect("the port exists");
    let mut first =
        r.ed.open_event_tap(EventTapId(2), port(1, 0), TapAlign::Raw, 64)
            .expect("the port exists");
    commit(&mut r);
    assert_eq!(tap_ops(&r), 2);
    render(&mut r, &[100], 100);
    assert_eq!(drain(&mut wide), vec![at(10, on_port(on(1), 1))]);
    script(&mut r, 1, s.clone().ports(1));
    commit(&mut r);
    assert_eq!(tap_ops(&r), 1, "no op for the port that is gone");
    render(&mut r, &[100], 100);
    assert_eq!(drain(&mut wide), vec![], "the vanished port reads nothing");
    assert_eq!(wide.take_lost(), 0, "and that is not a loss");
    script(&mut r, 1, s.ports(2));
    commit(&mut r);
    assert_eq!(tap_ops(&r), 2);
    render(&mut r, &[100], 100);
    assert_eq!(
        drain(&mut wide),
        vec![at(250, on_port(on(3), 1))],
        "the restored port resumes"
    );
    assert_eq!(
        drain(&mut first).len(),
        3,
        "the other tap played through all of it"
    );
}

/// Removing the tapped node reads nothing and commits; a node inserted again
/// under the removed key is not read by the old tap.
#[test]
fn a_removed_node_reads_nothing_and_a_reused_key_is_not_read() {
    let mut r = rig(1, false, 0);
    script(&mut r, 1, Script::new(vec![(10, on(1))]));
    let mut tap =
        r.ed.open_event_tap(EventTapId(1), port(1, 0), TapAlign::Raw, 64)
            .expect("the port exists");
    commit(&mut r);
    render(&mut r, &[100], 100);
    assert_eq!(drain(&mut tap).len(), 1);
    r.ed.remove(NodeKey(1));
    commit(&mut r);
    assert_eq!(tap_ops(&r), 0);
    script(&mut r, 1, Script::new(vec![(150, on(7))]));
    commit(&mut r);
    render(&mut r, &[100], 100);
    assert_eq!(
        drain(&mut tap),
        vec![],
        "the tap followed the removal, not the key"
    );
    assert_eq!(tap.take_lost(), 0);
}

/// A source behind a node of declared latency L = 15, beside an output path of
/// latency M = 40: the raw tap reads the script's own frames, the aligned one
/// the script's plus the compiler's gap (M - L = 25, an analytic figure); a
/// source with no latency is M late when aligned. Aligned taps cost an event
/// delay each, raw ones none.
#[test]
fn aligned_against_raw_on_a_latent_path() {
    for sizes in [&[64usize][..], &[100], &[1, 7, 64]] {
        let mut r = rig(1, false, 40);
        let events = vec![(3, on(1)), (10, on(2)), (100, on(3))];
        script(&mut r, 1, Script::new(events.clone()));
        script(&mut r, 2, Script::new(events.clone()).latency(15));
        let open = |r: &mut Rig, id, node, align| {
            r.ed.open_event_tap(EventTapId(id), port(node, 0), align, 64)
                .expect("the port exists")
        };
        let mut raw1 = open(&mut r, 1, 1, TapAlign::Raw);
        let mut aligned1 = open(&mut r, 2, 1, TapAlign::ToOutput);
        let mut raw2 = open(&mut r, 3, 2, TapAlign::Raw);
        let mut aligned2 = open(&mut r, 4, 2, TapAlign::ToOutput);
        commit(&mut r);
        assert_eq!(tap_ops(&r), 4);
        assert_eq!(event_delays(&r), 2, "one delay for each aligned tap");
        render(&mut r, sizes, 300);
        let shifted = |by: u64| -> Vec<TapEvent> {
            events
                .iter()
                .map(|&(f, u)| at(f + by, on_port(u, 0)))
                .collect()
        };
        assert_eq!(drain(&mut raw1), shifted(0), "{sizes:?}");
        assert_eq!(drain(&mut aligned1), shifted(40), "{sizes:?}");
        assert_eq!(drain(&mut raw2), shifted(0), "{sizes:?}");
        assert_eq!(drain(&mut aligned2), shifted(25), "{sizes:?}");
    }
}

/// A closed tap leaves no op (and no delay), nothing is written for it, a fork
/// carries none, opening an id again replaces the tap, and a reader dropped
/// first is reaped on the control side.
#[test]
fn closed_fork_reopen_and_reap() {
    let mut r = rig(1, false, 0);
    script(&mut r, 1, Script::new(vec![(10, on(1)), (110, on(2))]));
    let open = |r: &mut Rig, id| {
        r.ed.open_event_tap(EventTapId(id), port(1, 0), TapAlign::Raw, 64)
            .expect("the port exists")
    };
    let closed = open(&mut r, 1);
    commit(&mut r);
    assert_eq!(tap_ops(&r), 1);
    r.ed.close_event_tap(closed.id()).unwrap();
    assert_eq!(
        r.ed.close_event_tap(closed.id()),
        Err(CloseError::EventTap(closed.id())),
        "a closed tap is not open"
    );
    assert_eq!(
        r.ed.close_event_tap(EventTapId(99)),
        Err(CloseError::EventTap(EventTapId(99))),
        "a tap never opened is not open"
    );
    commit(&mut r);
    assert_eq!(tap_ops(&r), 0, "a closed tap leaves no op");
    drop(closed);

    // A fork carries no event tap.
    let live = open(&mut r, 2);
    commit(&mut r);
    let (fed, _fexec) =
        r.ed.fork(ForkTarget::Master, ForkMode::Offline, prepare(MAX_BLOCK))
            .expect("forks");
    assert!(fed.spec().topology.event_taps.is_empty());

    // Opening an id again replaces the tap.
    let mut second = open(&mut r, 2);
    commit(&mut r);
    render(&mut r, &[100], 100);
    assert_eq!(live.available(), 0, "the old reader is fed no more");
    assert_eq!(drain(&mut second).len(), 1);

    // An id opened twice between commits: the second replaces the first.
    let stale = open(&mut r, 5);
    let mut fresh = open(&mut r, 5);
    commit(&mut r);
    render(&mut r, &[100], 100);
    assert_eq!(stale.available(), 0, "the replaced reader is never fed");
    assert_eq!(drain(&mut fresh), vec![at(110, on_port(on(2), 0))]);
    r.ed.close_event_tap(EventTapId(5)).unwrap();
    drop((stale, fresh));

    // A reader dropped first: the tap writes nothing until it is reaped.
    let keep = open(&mut r, 3);
    let gone = open(&mut r, 4);
    commit(&mut r);
    drop(gone);
    drop(live);
    assert_eq!(
        r.ed.reap_event_taps(),
        1,
        "the dropped reader's tap; the replaced one's id is the new reader's"
    );
    commit(&mut r);
    assert_eq!(tap_ops(&r), 2, "the others stay");
    assert_eq!(r.ed.reap_event_taps(), 0);
    assert_eq!(keep.id(), EventTapId(3));
}

/// A reader dropped on the audio thread is a bug the debug build reports.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "an event tap reader dropped on the audio thread")]
fn a_reader_dropped_on_the_audio_thread_panics() {
    let mut r = rig(1, false, 0);
    script(&mut r, 1, Script::new(vec![]));
    let tap =
        r.ed.open_event_tap(EventTapId(1), port(1, 0), TapAlign::Raw, 8)
            .expect("the port exists");
    let _inside = tutti_rt::RtScope::enter();
    drop(tap);
}

/// Only MIDI is copied: a port carrying a `Resync` every block beside its notes
/// gives the notes alone, and the graph event is neither copied nor counted.
#[test]
fn only_midi_is_copied() {
    let mut r = rig(1, false, 0);
    script(
        &mut r,
        1,
        Script::new(vec![(10, on(1)), (70, on(2))]).resync(),
    );
    let mut tap =
        r.ed.open_event_tap(EventTapId(1), port(1, 0), TapAlign::Raw, 2)
            .expect("the port exists");
    commit(&mut r);
    render(&mut r, &[64], 128);
    assert_eq!(
        drain(&mut tap),
        vec![at(10, on_port(on(1), 0)), at(70, on_port(on(2), 0))]
    );
    assert_eq!(tap.take_lost(), 0, "a graph event does not fill the ring");
}

/// Opening refuses a node or a port that does not exist now, edits not yet
/// committed included, and declares nothing. An audio output is not an event
/// port.
#[test]
fn opening_refuses_a_port_that_does_not_exist_now() {
    let mut r = rig(1, false, 0);
    script(&mut r, 1, Script::new(vec![]).ports(2));
    let open = |r: &mut Rig, p: EventOut| {
        r.ed.open_event_tap(EventTapId(9), p, TapAlign::Raw, 8)
            .err()
    };
    assert_eq!(
        open(&mut r, port(1, 2)),
        Some(MissingSource::EventPort {
            node: NodeKey(1),
            port: 2,
            width: 2
        })
    );
    assert_eq!(
        open(&mut r, port(77, 0)),
        Some(MissingSource::Node { node: NodeKey(77) })
    );
    assert_eq!(
        open(&mut r, port(91, 0)),
        Some(MissingSource::EventPort {
            node: NodeKey(91),
            port: 0,
            width: 0
        }),
        "a node with audio outputs and no event output"
    );
    assert!(r.ed.spec().topology.event_taps.is_empty());
    assert_eq!(open(&mut r, port(1, 1)), None, "it exists, uncommitted");
}

/// Six scripted nodes of two ports each, every port tapped (raw and aligned
/// alternately) on a path whose output time is 40 frames.
fn busy(r: &mut Rig) -> Vec<EventTapReader> {
    let mut taps = Vec::new();
    for k in 1..=6u64 {
        let events: Vec<(u64, Ump)> = (0..40u64)
            .map(|i| (i * (9 + k) + k, on((i % 100) as u32)))
            .collect();
        script(r, k, Script::new(events).ports(2).latency(3 * k as usize));
        for p in 0..2u16 {
            let align = if (k + u64::from(p)) % 2 == 0 {
                TapAlign::Raw
            } else {
                TapAlign::ToOutput
            };
            let id = EventTapId((k * 2) as u32 + u32::from(p));
            taps.push(
                r.ed.open_event_tap(id, port(k, p), align, 1 << 10)
                    .expect("the port exists"),
            );
        }
    }
    commit(r);
    taps
}

fn run_busy(r: &mut Rig) -> Vec<Vec<TapEvent>> {
    let mut taps = busy(r);
    assert_eq!(r.exec.is_parallel(), r.gated, "a pool was set iff gated");
    render(r, &[100, 37, 128], 600);
    let got: Vec<Vec<TapEvent>> = taps.iter_mut().map(drain).collect();
    for t in &taps {
        assert_eq!(t.take_lost(), 0);
    }
    got
}

/// The pooled render's taps equal the serial one's, event for event, at
/// several worker counts with every participant inside `assert_no_alloc`; the
/// serial run itself is checked against the script and the compensation.
#[test]
fn the_parallel_event_taps_are_bit_identical_and_allocation_free() {
    let mut s = rig(1, false, 40);
    let serial = run_busy(&mut s);
    assert!(
        serial.iter().all(|t| !t.is_empty()),
        "every tap carries events"
    );
    // Tap 0 is node 1 port 0, aligned (1 + 0 is odd): the script's events
    // shifted by the gap between the output time (40) and its latency (3).
    let first: Vec<TapEvent> = (0..40u64)
        .map(|i| (i * 10 + 1 + 37, on((i % 100) as u32)))
        .filter(|&(f, _)| f < 600)
        .map(|(f, u)| at(f, on_port(u, 0)))
        .collect();
    assert_eq!(serial[0], first);
    for workers in [2, 3, 4] {
        let mut p = rig(workers, true, 40);
        assert_eq!(serial, run_busy(&mut p), "{workers} workers");
    }
}
