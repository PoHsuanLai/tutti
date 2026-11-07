//! `NoteLedger` through real graphs: a refused note-off is paid, no note is
//! left sounding whatever the port refused, a full table counts what it
//! cannot keep, and the clip node's reset, fork and release handshake leave
//! its notes ended.
//!
//! Every figure is counted by hand from the scenario or is an invariant of
//! the run (nothing sounds after a stop, every note-on has its note-off); no
//! test compares against another render. The clip plays at 120 BPM and
//! 48 kHz, so a beat is 24,000 frames.
//!
//! Mutations (each run, then restored):
//! - `NoteLedger::block` without `pay`: the refused offs are never sent.
//!   `a_refused_off_is_paid_at_the_first_frame_of_the_next_block`,
//!   `a_ledger_relay_strands_no_note_under_random_refusal` and
//!   `a_clip_leaves_nothing_sounding_after_a_stop` fail.
//! - `Sending::off` leaving an off the port refused unowed (no `owe`): the
//!   same three fail.
//! - `Sending::on` pushing the on of a full table anyway (no `withhold`
//!   return): `a_full_table_withholds_what_it_cannot_keep` and
//!   `a_small_table_withholds_the_on_it_cannot_keep` fail.
//! - `MidiClipNode::process` not answering a request (no `answered` store):
//!   the handshake tests fail. Not calling `release_all` for a request: the
//!   rewire after the drain leaves the note on A.
//! - `ClipFork` sharing the live `asked` and `answered` counters:
//!   `a_fork_does_not_answer_the_lives_ticket` fails.
//! - `MidiClipNode::reset` without `forget`:
//!   `a_reset_node_forgets_the_notes_it_held` fails.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use tutti_graph::contract::Direct;
use tutti_graph::{
    Cx, Editor, Event, EventEdge, EventIn, EventKind, EventOut, Executor, ForkCause, ForkMode,
    ForkSource, ForkTarget, Forked, IntoNode, Io, Node, NodeParts, Offset, Prepare, Shape, Status,
    Transport, Unforkable,
};
use tutti_midi_runtime::{MidiClipNode, NoteLedger, TimedMidiEvent};
use tutti_midi_types::MidiEvent;
use tutti_types::graph::{Edge, InPort, OutPort, Source};
use tutti_types::{Beat, Bpm, ChannelLayout, Latency, NodeKey, SampleRate, Samples};
use tutti_ump::{CCNumber, MidiChannel, MidiGroup, Ump};

const FPB: f64 = 24_000.0;

fn beat_of(frame: u64) -> Beat {
    Beat(frame as f64 / FPB)
}

fn rolling(frame: u64) -> Transport {
    Transport::new(true, Bpm(120.0), beat_of(frame), None)
}

fn stopped(frame: u64) -> Transport {
    Transport::new(false, Bpm(120.0), beat_of(frame), None)
}

fn on(n: u8) -> MidiEvent {
    MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, n, 0xFFFF)
}

fn off(n: u8) -> MidiEvent {
    MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, n, 0)
}

fn at(frame: u64, e: MidiEvent) -> TimedMidiEvent {
    TimedMidiEvent::new(beat_of(frame), e)
}

/// What a sink heard: the keys it holds, offs for keys it does not hold
/// (orphans), and the frame each off arrived on.
#[derive(Default)]
struct Tally {
    held: BTreeSet<u32>,
    orphans: usize,
    ons: usize,
    offs: usize,
    off_frames: Vec<u64>,
}

type Shared = Arc<Mutex<Tally>>;

struct Sink {
    tally: Shared,
    audio_in: bool,
    audio_out: bool,
}

impl Sink {
    fn new(tally: &Shared) -> Self {
        Self {
            tally: Arc::clone(tally),
            audio_in: false,
            audio_out: false,
        }
    }
}

impl Node for Sink {
    fn shape(&self) -> Shape {
        let layout = |on| {
            if on {
                ChannelLayout::MONO
            } else {
                ChannelLayout::EMPTY
            }
        };
        Shape::audio(layout(self.audio_in), layout(self.audio_out)).with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let mut t = self.tally.lock().unwrap();
        for e in io.events(0) {
            let EventKind::Midi(Ump(w)) = e.kind else {
                continue;
            };
            let key = w[0] & 0x0f0f_7f00;
            let frame = cx.env.frame.get() + u64::from(e.offset.get());
            if e.is_note_off() {
                t.off_frames.push(frame);
                t.offs += 1;
                if !t.held.remove(&key) {
                    t.orphans += 1;
                }
            } else if (w[0] >> 20) & 0xf == 0x9 {
                t.ons += 1;
                t.held.insert(key);
            }
        }
        if self.audio_out {
            io.output(0).fill(0.0);
            return Status::Modified;
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// A sink a fork can copy: the fork hears into a tally of its own.
struct ForkableSink {
    sink: Sink,
    forks: Arc<Mutex<Vec<Shared>>>,
}

struct SinkFork(Arc<Mutex<Vec<Shared>>>);

impl ForkSource for SinkFork {
    fn fork(&self, _: ForkMode) -> Result<Forked, ForkCause> {
        let tally = Shared::default();
        self.0.lock().unwrap().push(Arc::clone(&tally));
        Ok(Forked::new(Box::new(Sink {
            tally,
            audio_in: false,
            audio_out: true,
        })))
    }
}

impl IntoNode for ForkableSink {
    type Controls = ();
    fn into_parts(self) -> NodeParts<()> {
        NodeParts {
            node: Box::new(self.sink),
            controls: (),
            fork: Some(Box::new(SinkFork(self.forks))),
        }
    }
}

/// A silent audio source with a declared latency: the compiler delays every
/// other path into a node it shares with a sink.
struct Latent(usize);

impl Node for Latent {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
            .with_latency(Latency::new(Samples(self.0)))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

fn connect(ed: &mut Editor, from: u64, to: u64) {
    ed.spec_mut().connect_events(
        EventIn {
            node: NodeKey(to),
            port: 0,
        },
        EventEdge::Direct(EventOut {
            node: NodeKey(from),
            port: 0,
        }),
    );
}

fn disconnect(ed: &mut Editor, from: u64, to: u64) {
    assert!(ed.spec_mut().disconnect_events(
        EventIn {
            node: NodeKey(to),
            port: 0
        },
        EventOut {
            node: NodeKey(from),
            port: 0
        }
    ));
}

fn sink(ed: &mut Editor, key: u64) -> Shared {
    let tally = Shared::default();
    ed.insert(NodeKey(key), "sink", Unforkable(Sink::new(&tally)));
    tally
}

/// `blocks` blocks of `block` frames from frame `from`, rolling; returns the
/// next frame.
fn run(exec: &mut Executor, from: u64, blocks: u64, block: usize) -> u64 {
    (0..blocks).fold(from, |f, _| {
        exec.process(block, &rolling(f), &[], &mut []);
        f + block as u64
    })
}

/// `run` for a graph with one audio output, which nobody reads.
fn run_out(exec: &mut Executor, from: u64, blocks: u64) -> u64 {
    (0..blocks).fold(from, |f, _| {
        let mut out = [0.0f32; 256];
        exec.process(256, &rolling(f), &[], &mut [&mut out[..]]);
        f + 256
    })
}

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

// ---------------------------------------------------------------------------
// A relay through the ledger.
// ---------------------------------------------------------------------------

/// Emits `blocks[k]` on block `k` (all on the block's first frame), from a
/// port big enough for all of it.
struct Script {
    blocks: Vec<Vec<Ump>>,
    k: usize,
}

impl Node for Script {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY)
            .with_events(0, 1)
            .with_event_capacity(2048)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        if let Some(block) = self.blocks.get(self.k) {
            let out = io.event_out(0);
            for u in block {
                let _ = out.push(Event::midi(Offset::ZERO, u.0));
            }
        }
        self.k += 1;
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// Passes every event on through a ledger, into a port of `capacity` events.
struct Relay {
    ledger: Option<NoteLedger<()>>,
    capacity: u32,
}

impl Node for Relay {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY)
            .with_events(1, 1)
            .with_event_capacity(self.capacity)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let events = io.events(0);
        let Some(first) = cx.env.offset(0) else {
            return Status::Modified;
        };
        let out = io.event_out(0);
        match &mut self.ledger {
            Some(ledger) => {
                let mut sending = ledger.block(out, first);
                for e in events {
                    if let EventKind::Midi(u) = e.kind {
                        sending.send(e.offset, u, None);
                    }
                }
            }
            None => {
                for e in events {
                    let _ = out.push(*e);
                }
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {
        if let Some(ledger) = &mut self.ledger {
            ledger.forget();
        }
    }
}

fn ump_on(n: u32) -> Ump {
    let (c, n) = (((n / 128) % 16) as u8, (n % 128) as u8);
    Ump(MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::of(c), n, 0xFFFF).data)
}

fn ump_off(n: u32) -> Ump {
    let (c, n) = (((n / 128) % 16) as u8, (n % 128) as u8);
    Ump(MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::of(c), n, 0).data)
}

fn ump_cc(v: u32) -> Ump {
    let cc = MidiEvent::cc(MidiGroup::FIRST, MidiChannel::FIRST, CCNumber::of(7), v);
    Ump(cc.data)
}

/// `script` through `relay` into a sink, in 64-frame blocks; `blocks` of
/// them.
fn through(script: Vec<Vec<Ump>>, relay: Relay, blocks: u64) -> (Shared, Executor) {
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(64)));
    ed.insert(
        NodeKey(1),
        "script",
        Unforkable(Script {
            blocks: script,
            k: 0,
        }),
    );
    ed.insert(NodeKey(2), "relay", Unforkable(relay));
    let tally = sink(&mut ed, 3);
    connect(&mut ed, 1, 2);
    connect(&mut ed, 2, 3);
    ed.commit().unwrap();
    run(&mut exec, 0, blocks, 64);
    (tally, exec)
}

/// 256 notes start in block 0. Block 1 sends 48 controllers and the 256
/// offs into a port of 256 events with room for 296 note-offs
/// (`note_off_room`: 256 + 32 + 8): 48 + 248 fit, 8 offs are refused. They
/// are owed, and all 8 go out on the first frame of block 2, which is frame
/// 128 of 64-frame blocks.
#[test]
fn a_refused_off_is_paid_at_the_first_frame_of_the_next_block() {
    let script = vec![
        (0..256).map(ump_on).collect(),
        (0..48).map(ump_cc).chain((0..256).map(ump_off)).collect(),
    ];
    let relay = Relay {
        ledger: Some(NoteLedger::new(2048)),
        capacity: 256,
    };
    let (tally, exec) = through(script, relay, 6);
    let t = tally.lock().unwrap();
    assert_eq!((t.ons, t.offs), (256, 256));
    assert!(t.held.is_empty());
    let late: Vec<u64> = t.off_frames.iter().copied().filter(|&f| f == 128).collect();
    assert_eq!(late.len(), 8, "{:?}", t.off_frames);
    // Refused once each and delivered later: the loss nets zero.
    assert_eq!(exec.dropped_event_count(), 8);
    assert_eq!(exec.lost_event_count(), 0);
}

/// A table of 4 keys against 6 notes: the last 2 note-ons are withheld and
/// counted (2 in the dropped count, never silent), the 4 that started end,
/// and the table is free again afterwards, so a later note starts. A relay's
/// off carries no owner, so the 2 offs of the notes that never started pass
/// through as orphans.
#[test]
fn a_small_table_withholds_the_on_it_cannot_keep() {
    let script = vec![
        (0..6).map(ump_on).collect(),
        (0..6).map(ump_off).collect(),
        vec![ump_on(7)],
        vec![ump_off(7)],
    ];
    let relay = Relay {
        ledger: Some(NoteLedger::new(4)),
        capacity: 256,
    };
    let (tally, exec) = through(script, relay, 6);
    let t = tally.lock().unwrap();
    assert_eq!((t.ons, t.offs), (5, 7), "4 + the later note, 6 + 1 offs");
    assert_eq!(t.orphans, 2);
    assert!(t.held.is_empty());
    assert_eq!(exec.dropped_event_count(), 2);
}

/// 20 random blocks of toggling 12 keys (a held key's next event is its off)
/// among controller filler that saturates an 8-event port, then every held
/// key is ended and the port drains for 12 blocks. Returns the keys the sink
/// still holds.
fn random_relay(seed: u64, ledger: bool) -> usize {
    let mut r = Rng((seed * 0x9E37_79B9) | 1);
    let mut sender_held = [false; 12];
    let mut blocks: Vec<Vec<Ump>> = (0..20)
        .map(|_| {
            (0..r.below(30))
                .map(|_| {
                    if r.below(3) == 0 {
                        ump_cc(r.below(100) as u32)
                    } else {
                        let k = r.below(12) as usize;
                        sender_held[k] = !sender_held[k];
                        let key = k as u32;
                        if sender_held[k] {
                            ump_on(key)
                        } else {
                            ump_off(key)
                        }
                    }
                })
                .collect()
        })
        .collect();
    let last = (0..40)
        .map(ump_cc)
        .chain((0..12u32).filter(|&k| sender_held[k as usize]).map(ump_off))
        .collect();
    blocks.push(last);
    let relay = Relay {
        ledger: ledger.then(|| NoteLedger::new(64)),
        capacity: 8,
    };
    let (tally, _) = through(blocks, relay, 33);
    let held = tally.lock().unwrap().held.len();
    held
}

/// Whatever the port refuses, a relay through the ledger leaves no note
/// sounding. The plain relay is the control: it strands notes in this very
/// scenario, so the scenario does refuse.
#[test]
fn a_ledger_relay_strands_no_note_under_random_refusal() {
    const SEEDS: u64 = 3_000;
    for seed in 1..=SEEDS {
        assert_eq!(random_relay(seed, true), 0, "seed {seed}");
    }
    let stranded = (1..=SEEDS).filter(|&s| random_relay(s, false) > 0).count();
    assert!(stranded > 0, "the scenario never refused an off");
}

// ---------------------------------------------------------------------------
// The clip node.
// ---------------------------------------------------------------------------

/// A clip into a sink in 64-frame blocks (a port of 256 events).
fn clip_rig(events: Vec<TimedMidiEvent>) -> (Editor, Executor, Shared) {
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(64)));
    ed.insert(NodeKey(1), "clip", MidiClipNode::new(events));
    let tally = sink(&mut ed, 2);
    connect(&mut ed, 1, 2);
    ed.commit().unwrap();
    (ed, exec, tally)
}

/// Random notes on 384 keys, most starting on a few shared frames so that a
/// block holds more than the port takes, played through random seeks and
/// stops, then stopped for 20 blocks. Returns the sink's tally and the
/// events the port refused.
fn random_clip(seed: u64) -> (Shared, u64) {
    let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let starts = [0u64, 10, 70, 500];
    let events = (0..100 + r.below(500))
        .flat_map(|_| {
            let g = MidiGroup::of(r.below(4) as u8);
            let c = MidiChannel::of(r.below(4) as u8);
            let n = r.below(24) as u8;
            let start = if r.below(4) == 0 {
                r.below(1_500)
            } else {
                starts[r.below(4) as usize]
            };
            let end = start + 1 + r.below(1_200);
            [
                at(start, MidiEvent::note_on(g, c, n, 0xFFFF)),
                at(end, MidiEvent::note_off(g, c, n, 0)),
            ]
        })
        .collect();
    let (_ed, mut exec, tally) = clip_rig(events);
    let mut frame = 0;
    for _ in 0..r.below(6) + 2 {
        frame = run(&mut exec, frame, 1 + r.below(8), 64);
        match r.below(3) {
            0 => frame = r.below(1_900),
            1 => (0..1 + r.below(3)).for_each(|_| exec.process(64, &stopped(frame), &[], &mut [])),
            _ => {}
        }
    }
    (0..20).for_each(|_| exec.process(64, &stopped(frame), &[], &mut []));
    (tally, exec.dropped_event_count())
}

/// Whatever the port refused, nothing sounds after a stop, every note-on the
/// sink heard has its note-off, and the sink never hears an off for a note
/// it did not start.
#[test]
fn a_clip_leaves_nothing_sounding_after_a_stop() {
    let mut refused = 0;
    for seed in 1..=1_000 {
        let (tally, dropped) = random_clip(seed);
        let t = tally.lock().unwrap();
        assert!(t.held.is_empty(), "seed {seed}: {:?}", t.held);
        assert_eq!(t.ons, t.offs, "seed {seed}");
        assert_eq!(t.orphans, 0, "seed {seed}");
        refused += dropped;
    }
    assert!(refused > 0, "the scenario never refused an event");
}

/// 2,100 notes sound at once across two groups, one starting on each frame
/// from frame 0, against a table of 2,048 keys. The first 2,048 start. The
/// last 52 are withheld, each counted once in the dropped count and charged
/// to the clip's port; their offs are swallowed, not counted again. The 2,048
/// that started all end (frame 100,000 on), nothing is orphaned, and once
/// the table has room a later note starts.
#[test]
fn a_full_table_withholds_what_it_cannot_keep() {
    let key = |i: u32| {
        let (g, rest) = (i / 2_048, i % 2_048);
        (
            MidiGroup::of(g as u8),
            MidiChannel::of((rest / 128) as u8),
            (rest % 128) as u8,
        )
    };
    let note = |i: u32, start: u64, end: u64| {
        let (g, c, n) = key(i);
        [
            at(start, MidiEvent::note_on(g, c, n, 0xFFFF)),
            at(end, MidiEvent::note_off(g, c, n, 0)),
        ]
    };
    let events = (0..2_100u32)
        .flat_map(|i| note(i, u64::from(i), 100_000 + u64::from(i)))
        .chain(note(2_100, 104_000, 104_100))
        .collect();
    let (mut ed, mut exec, tally) = clip_rig(events);
    run(&mut exec, 0, 1_700, 64); // past frame 108,800
    assert_eq!(exec.dropped_event_count(), 52);
    let losses: Vec<_> = ed.take_event_losses().collect();
    assert_eq!(losses.iter().map(|l| l.count).sum::<u32>(), 52);
    assert!(
        losses
            .iter()
            .all(|l| l.node == NodeKey(1) && l.port == Some(0))
    );
    let t = tally.lock().unwrap();
    assert_eq!((t.ons, t.offs), (2_049, 2_049));
    assert_eq!(t.orphans, 0);
    assert!(t.held.is_empty());
}

// ---------------------------------------------------------------------------
// The release handshake.
// ---------------------------------------------------------------------------

/// A clip holds note 60 from frame 0 to frame 100,000 and feeds sink A.
fn holding_clip() -> (
    Editor,
    Executor,
    tutti_midi_runtime::MidiClipControls,
    Shared,
    Shared,
) {
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(256)));
    let controls = ed.insert(
        NodeKey(1),
        "clip",
        MidiClipNode::new([at(0, on(60)), at(100_000, off(60))]),
    );
    let a = sink(&mut ed, 2);
    let b = sink(&mut ed, 3);
    connect(&mut ed, 1, 2);
    ed.commit().unwrap();
    (ed, exec, controls, a, b)
}

/// Re-aims the clip from A to B and runs past frame 100,000.
fn rewire(ed: &mut Editor, exec: &mut Executor, from: u64) {
    disconnect(ed, 1, 2);
    connect(ed, 1, 3);
    ed.commit().unwrap();
    run(exec, from, 400, 256);
}

/// A ticket is not drained before a block has run, and is after one.
#[test]
fn a_ticket_drains_once_a_block_has_answered() {
    let (_ed, mut exec, controls, a, _b) = holding_clip();
    let f = run(&mut exec, 0, 2, 256);
    assert_eq!(a.lock().unwrap().held.len(), 1);
    let ticket = controls.release_held();
    let ticket = controls.drained(ticket).expect_err("no block has run");
    run(&mut exec, f, 1, 256);
    let _proof = controls.drained(ticket).expect("a block answered");
    assert!(
        a.lock().unwrap().held.is_empty(),
        "the answer ended the note"
    );
}

/// Without the handshake, re-aiming a clip that holds a note strands it on
/// A, and B hears one off for a note it never started. This is the open
/// part of the audit's item 7: the source cannot reach an edge that is gone.
/// The edge-level fix will change this test.
#[test]
fn a_rewire_without_the_handshake_strands_the_note() {
    let (mut ed, mut exec, _controls, a, b) = holding_clip();
    let f = run(&mut exec, 0, 2, 256);
    rewire(&mut ed, &mut exec, f);
    assert_eq!(a.lock().unwrap().held.len(), 1);
    assert_eq!(b.lock().unwrap().orphans, 1);
}

/// With the handshake, nothing is stranded and B hears nothing: the clip's
/// own off at frame 100,000 is swallowed, its note having been ended.
#[test]
fn a_rewire_after_the_drain_leaves_nothing() {
    let (mut ed, mut exec, controls, a, b) = holding_clip();
    let f = run(&mut exec, 0, 2, 256);
    let ticket = controls.release_held();
    let f = run(&mut exec, f, 1, 256);
    let _proof = controls.drained(ticket).expect("a block answered");
    rewire(&mut ed, &mut exec, f);
    assert!(a.lock().unwrap().held.is_empty());
    let b = b.lock().unwrap();
    assert_eq!((b.ons, b.offs, b.orphans), (0, 0, 0));
}

/// Sink A also takes audio from a node of 1,024 frames of latency, so the
/// compiler delays the event edge into A by 1,024 frames (4 blocks of 256).
/// The off the drain put on the edge has not reached A when the ticket is
/// drained; cutting the edge then still delivers it, because the graph
/// flushes the events in flight on a delay that goes away. It arrives at the
/// cut, frame 2,304 here, instead of at 3,072.
#[test]
fn the_graph_delivers_the_drained_off_on_a_delayed_edge() {
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(256)));
    let controls = ed.insert(
        NodeKey(1),
        "clip",
        MidiClipNode::new([at(0, on(60)), at(100_000, off(60))]),
    );
    ed.insert(NodeKey(4), "latent", Unforkable(Latent(1_024)));
    let a = Shared::default();
    ed.insert(
        NodeKey(2),
        "sink",
        Unforkable(Sink {
            audio_in: true,
            ..Sink::new(&a)
        }),
    );
    sink(&mut ed, 3);
    connect(&mut ed, 1, 2);
    ed.spec_mut().topology.edges.insert(
        InPort {
            node: NodeKey(2),
            port: 0,
        },
        Edge::Direct(Source::Node(OutPort {
            node: NodeKey(4),
            port: 0,
        })),
    );
    ed.commit().unwrap();
    let f = run(&mut exec, 0, 8, 256);
    assert_eq!(a.lock().unwrap().held.len(), 1, "the note reached A");
    let ticket = controls.release_held();
    let f = run(&mut exec, f, 1, 256);
    let _proof = controls.drained(ticket).expect("a block answered");
    assert_eq!(f, 2_304);
    rewire(&mut ed, &mut exec, f);
    let a = a.lock().unwrap();
    assert!(a.held.is_empty());
    assert_eq!(a.off_frames, vec![2_304]);
}

// ---------------------------------------------------------------------------
// Fork and reset.
// ---------------------------------------------------------------------------

/// A clip with a note from frame 10 to frame 300, into a sink with audio
/// output so the sink can be a fork target.
fn forkable_rig() -> (
    Editor,
    Executor,
    tutti_midi_runtime::MidiClipControls,
    Shared,
    Arc<Mutex<Vec<Shared>>>,
) {
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(256)));
    let controls = ed.insert(
        NodeKey(1),
        "clip",
        MidiClipNode::new([at(10, on(60)), at(300, off(60))]),
    );
    let tally = Shared::default();
    let forks = Arc::default();
    ed.insert(
        NodeKey(2),
        "sink",
        ForkableSink {
            sink: Sink {
                audio_out: true,
                ..Sink::new(&tally)
            },
            forks: Arc::clone(&forks),
        },
    );
    connect(&mut ed, 1, 2);
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort {
        node: NodeKey(2),
        port: 0,
    })];
    ed.commit().unwrap();
    (ed, exec, controls, tally, forks)
}

fn fork_of(ed: &Editor) -> (Editor, Executor) {
    let prepare = Prepare::new(SampleRate(48_000.0), Samples(256));
    ed.fork(ForkTarget::Node(NodeKey(2)), ForkMode::Offline, prepare)
        .expect("the clip and the sink fork")
}

/// The live clip holds note 60 when the graph is forked; the fork renders
/// from frame 256, inside the note. The fork starts with an empty table, so
/// the clip's off at frame 300 is swallowed there (its note never started in
/// the fork), while the live clip's off still arrives.
#[test]
fn a_fork_starts_with_an_empty_table() {
    let (ed, mut exec, _controls, live, forks) = forkable_rig();
    let f = run_out(&mut exec, 0, 1);
    assert_eq!(live.lock().unwrap().held.len(), 1);
    let (_fed, mut fexec) = fork_of(&ed);
    run_out(&mut fexec, f, 1);
    let fork = forks.lock().unwrap()[0].clone();
    {
        let fork = fork.lock().unwrap();
        assert_eq!((fork.ons, fork.offs, fork.orphans), (0, 0, 0));
    }
    run_out(&mut exec, f, 1);
    let live = live.lock().unwrap();
    assert_eq!((live.ons, live.offs), (1, 1));
    assert!(live.held.is_empty());
}

/// A fork's clip answers its own requests only. A ticket asked of the live
/// clip is not drained by rendering the fork, and is when the live graph runs
/// a block.
#[test]
fn a_fork_does_not_answer_the_lives_ticket() {
    let (ed, mut exec, controls, live, _forks) = forkable_rig();
    let f = run_out(&mut exec, 0, 1);
    let ticket = controls.release_held();
    let (_fed, mut fexec) = fork_of(&ed);
    run_out(&mut fexec, f, 3);
    let ticket = controls
        .drained(ticket)
        .expect_err("the fork is another node");
    assert_eq!(live.lock().unwrap().held.len(), 1);
    run_out(&mut exec, f, 1);
    let _proof = controls.drained(ticket).expect("the live block answered");
    assert!(live.lock().unwrap().held.is_empty());
}

/// A reset node has forgotten what it held: stopping the transport ends
/// nothing. The twin without the reset ends note 60 on the stopping block,
/// so the empty stop is the reset's doing.
#[test]
fn a_reset_node_forgets_the_notes_it_held() {
    let stop_after = |reset: bool| {
        let clip = MidiClipNode::new([at(10, on(60)), at(5_000, off(60))]);
        let mut d = Direct::new(clip, SampleRate(48_000.0), 256);
        d.set_transport(rolling(0));
        d.block();
        assert_eq!(d.events_out(0).len(), 1, "the note started");
        if reset {
            d.node.reset();
        }
        d.set_transport(stopped(256));
        d.block();
        d.events_out(0).len()
    };
    assert_eq!(stop_after(false), 1, "the stop ends the held note");
    assert_eq!(stop_after(true), 0);
}
