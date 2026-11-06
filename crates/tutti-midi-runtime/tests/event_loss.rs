//! A clip loses no event it can place, and none it cannot place goes
//! unreported or leaves a note sounding.
//!
//! Every figure is counted by hand from the clip below, not from another
//! render. The clip plays at 120 BPM and 48 kHz, so a beat is 24,000 frames.

use std::sync::{Arc, Mutex};

use tutti_core::{Beat, Bpm, NodeKey, SampleRate, Samples};
use tutti_graph::{
    Cx, Editor, EventEdge, EventIn, EventKind, EventLoss, EventOut, Executor, Io, Node, Prepare,
    Shape, Status, Transport, Ump, Unforkable,
};
use tutti_midi_runtime::{MidiClipNode, TimedMidiEvent};
use tutti_midi_types::{MidiChannel, MidiEvent, MidiGroup};

const FRAMES_PER_BEAT: f64 = 24_000.0;

/// `(note-ons, note-offs)` the sink received, and the keys it ends holding.
#[derive(Default)]
struct Tally {
    ons: usize,
    offs: usize,
    held: std::collections::BTreeSet<u32>,
}

struct Sink(Arc<Mutex<Tally>>);

impl Node for Sink {
    fn shape(&self) -> Shape {
        Shape::audio(
            tutti_core::ChannelLayout::EMPTY,
            tutti_core::ChannelLayout::EMPTY,
        )
        .with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _cx: &Cx<'_>, io: Io<'_>) -> Status {
        let mut t = self.0.lock().unwrap();
        for e in io.events(0) {
            let EventKind::Midi(Ump(w)) = e.kind else {
                panic!("a clip sends MIDI")
            };
            let key = w[0] & 0x000f_7f00; // channel and note
            if e.is_note_off() {
                t.offs += 1;
                t.held.remove(&key);
            } else {
                t.ons += 1;
                t.held.insert(key);
            }
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

fn beat_of(frame: u64) -> Beat {
    Beat(frame as f64 / FRAMES_PER_BEAT)
}

/// Note `i` as a key: 128 notes on each of 16 channels.
fn key(i: u32) -> (MidiChannel, u8) {
    (MidiChannel::new(((i / 128) % 16) as u8), (i % 128) as u8)
}

fn on(i: u32) -> MidiEvent {
    let (c, n) = key(i);
    MidiEvent::note_on(MidiGroup::FIRST, c, n, 0xFFFF)
}

fn off(i: u32) -> MidiEvent {
    let (c, n) = key(i);
    MidiEvent::note_off(MidiGroup::FIRST, c, n, 0)
}

fn at(frame: u64, event: MidiEvent) -> TimedMidiEvent {
    TimedMidiEvent::new(beat_of(frame), event)
}

struct Rig {
    ed: Editor,
    exec: Executor,
    tally: Arc<Mutex<Tally>>,
}

/// A clip at key 1 into the sink at key 2, blocks of at most `max`.
fn rig(events: impl IntoIterator<Item = TimedMidiEvent>, max: usize) -> Rig {
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(max)));
    ed.insert(NodeKey(1), "clip", MidiClipNode::new(events));
    let tally = Arc::new(Mutex::new(Tally::default()));
    ed.insert(NodeKey(2), "sink", Unforkable(Sink(Arc::clone(&tally))));
    ed.spec_mut().connect_events(
        EventIn {
            node: NodeKey(2),
            port: 0,
        },
        EventEdge::Direct(EventOut {
            node: NodeKey(1),
            port: 0,
        }),
    );
    ed.commit().expect("commits");
    Rig { ed, exec, tally }
}

impl Rig {
    /// Roll from frame 0 for `frames`, in blocks of `block`; returns the
    /// losses the editor was told of.
    fn roll(&mut self, frames: u64, block: usize) -> Vec<EventLoss> {
        let mut f = 0;
        while f < frames {
            let t = Transport::new(true, Bpm(120.0), beat_of(f), None);
            self.exec.process(block, &t, &[], &mut []);
            f += block as u64;
        }
        self.ed.take_event_losses().collect()
    }
}

/// The dense clip of the audit: 10,000 note pairs, an event every 1.5 frames
/// (32,000 events a second): the note-on on frame `3 i`, its note-off on
/// frame `3 i + 2`. Whatever the block, the port is sized for it, so every
/// one of the 20,000 events arrives and nothing is reported. At 512 frames
/// a block holds about 341 events, at 1,024 about 683, at 16,384 about 10,923:
/// each under that block's capacity (512, 1,024 and 16,384).
///
/// The same clip lost 25% at 512 frames, 62% at 1,024 and 97% at 16,384 with
/// the fixed 256.
///
/// Mutation: size the port at a fixed 256 (`capacity: 256` regardless of
/// `prepare`, in `MidiClipNode::prepare`) → the longer blocks lose events →
/// fails.
#[test]
fn a_dense_clip_loses_nothing_whatever_the_block() {
    let events = || {
        (0..10_000u32).flat_map(|i| {
            let f = 3 * u64::from(i);
            [at(f, on(i % 2048)), at(f + 2, off(i % 2048))]
        })
    };
    for block in [64usize, 256, 512, 1024, 2048, 4096, 16_384] {
        let mut r = rig(events(), block);
        let losses = r.roll(30_100, block);
        let t = r.tally.lock().unwrap();
        assert_eq!((t.ons, t.offs), (10_000, 10_000), "block {block}");
        assert!(t.held.is_empty(), "block {block}");
        assert!(losses.is_empty(), "block {block}: {losses:?}");
        assert_eq!(r.exec.dropped_event_count(), 0, "block {block}");
    }
}

/// A chord of 200 notes on frame 10 and off on frame 500, in a 1,024-frame
/// block (what an export renders at): 400 events in one block. All 200 start
/// and all 200 end; none is left sounding.
///
/// The same chord left 144 of the 200 sounding to the end of the file.
///
/// Mutation: as above (fixed 256) → the chord's note-offs are refused → fails.
#[test]
fn a_chord_ends_in_a_long_block() {
    let events = (0..200).flat_map(|i| [at(10, on(i)), at(500, off(i))]);
    let mut r = rig(events, 1024);
    assert!(r.roll(2048, 1024).is_empty());
    let t = r.tally.lock().unwrap();
    assert_eq!((t.ons, t.offs), (200, 200));
    assert!(t.held.is_empty());
}

/// A chord too big for a short block: 300 notes on frame 10 of 64-frame
/// blocks, whose port holds 256, off on frame 500. The port refuses the last
/// 44 note-ons; the clip never writes the note-offs of those notes. 256
/// notes start and 256 end, and nothing is left sounding or ended unstarted.
/// The loss is 44 note-ons refused and 44 note-offs withheld, each counted
/// once: 88, charged to the clip's port 0 in the blocks they were lost in.
///
/// The executor's counter used to read 44 here: the swallowed note-offs were
/// never counted.
///
/// Mutation: drop `out.withhold()` for the off of a refused note-on in
/// `HeldNotes::play` → the count is 44 → fails. Mutation: write the note-off
/// of a refused note-on anyway → 300 note-offs reach the sink → fails.
#[test]
fn a_loss_is_counted_exactly_and_leaves_no_orphan() {
    let events = (0..300).flat_map(|i| [at(10, on(i)), at(500, off(i))]);
    let mut r = rig(events, 64);
    let losses = r.roll(1024, 64);
    let t = r.tally.lock().unwrap();
    assert_eq!((t.ons, t.offs), (256, 256));
    assert!(t.held.is_empty());
    assert_eq!(r.exec.dropped_event_count(), 88);
    let sum: u32 = losses.iter().map(|l| l.count).sum();
    assert_eq!(sum, 88);
    assert!(
        losses
            .iter()
            .all(|l| l.node == NodeKey(1) && l.port == Some(0)),
        "{losses:?}"
    );
    assert_eq!(losses.len(), 2, "one block lost the note-ons, one the offs");
}

/// A note-off the port refuses stays owed and goes out in a following block,
/// with the clip still rolling. In 64-frame blocks (a port of 256, and room
/// for 296 note-offs) 200 notes start on frame 0 and end on frame 70, where
/// 200 other notes start on frame 65: 400 events in the block. The 200
/// note-ons land at 65, the first 96 of the 200 note-offs take the room past
/// 256, and the other 104 are refused. They are owed, and every one goes out
/// on the first frame of the next block; the second 200 notes still sound,
/// and their note-offs (frame 1,000) end them.
///
/// Before, a refused note-off was never retried while the transport
/// rolled: 104 notes stayed on until it stopped.
///
/// Mutation: skip `held.retry` in `process` → the 104 are never sent →
/// fails. Mutation: skip `owe` on a refused note-off → the same.
#[test]
fn a_refused_note_off_goes_out_in_a_following_block() {
    let first = (0..200).flat_map(|i| [at(0, on(i)), at(70, off(i))]);
    let second = (200..400).flat_map(|i| [at(65, on(i)), at(1_000, off(i))]);
    let mut r = rig(first.chain(second), 64);
    // Through the first 64 frames: the first 200 sound.
    let transport = |f: u64| Transport::new(true, Bpm(120.0), beat_of(f), None);
    r.exec.process(64, &transport(0), &[], &mut []);
    {
        let t = r.tally.lock().unwrap();
        assert_eq!((t.ons, t.offs, t.held.len()), (200, 0, 200));
    }
    // Frames 64..128: 200 note-ons, 96 note-offs landed, 104 refused.
    r.exec.process(64, &transport(64), &[], &mut []);
    {
        let t = r.tally.lock().unwrap();
        assert_eq!((t.ons, t.offs), (400, 96));
    }
    assert_eq!(r.exec.dropped_event_count(), 104);
    // The next block: the 104 owed go out on its first frame.
    r.exec.process(64, &transport(128), &[], &mut []);
    {
        let t = r.tally.lock().unwrap();
        assert_eq!((t.ons, t.offs), (400, 200));
        assert_eq!(t.held.len(), 200, "the second chord still sounds");
    }
    // And it ends on its own frame, with nothing more refused.
    let mut f = 192;
    while f < 1_088 {
        r.exec.process(64, &transport(f), &[], &mut []);
        f += 64;
    }
    let t = r.tally.lock().unwrap();
    assert_eq!((t.ons, t.offs), (400, 400));
    assert!(t.held.is_empty());
    assert_eq!(r.exec.dropped_event_count(), 104);
}

/// The port is sized from the block the node was prepared for, and again
/// after a re-prepare: 256 at 64 frames, one a frame after that, the cap at
/// 1,048,576.
///
/// Mutation: ignore `prepare` in `MidiClipNode::prepare` (leave the
/// capacity at its floor) → 1,024 reads 256 → fails.
#[test]
fn the_port_follows_the_block() {
    for (block, want) in [
        (1usize, 256u32),
        (64, 256),
        (256, 256),
        (1024, 1024),
        (8192, 8192),
        (1 << 20, 16_384),
    ] {
        let mut node = MidiClipNode::new([]);
        node.prepare(&Prepare::new(SampleRate(48_000.0), Samples(block)));
        assert_eq!(
            node.shape().event_capacity.map(|n| n.get()),
            Some(want),
            "block {block}"
        );
    }
}

/// The hardware input is sized from the block it is prepared for: 512 events
/// up to a 2,048-frame block (one in four frames below that), a quarter of
/// the block's frames past it, up to 4,096 at 16,384; and its wire is told
/// (`MidiIn::reserve`), so the drain takes as many.
///
/// Mutation: ignore `prepare` in `MidiInputNode::prepare` → 8,192 reads 512
/// and the wire is told nothing → fails.
#[test]
fn the_hardware_input_follows_the_block() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tutti_midi_runtime::MidiInputNode;
    use tutti_midi_types::MidiIn;

    struct Wire(Arc<AtomicUsize>);
    impl MidiIn for Wire {
        fn poll_block(&mut self, _: usize, _: &mut [MidiEvent]) -> usize {
            0
        }
        fn reserve(&mut self, events: usize) {
            self.0.store(events, Ordering::Relaxed);
        }
    }

    for (block, want) in [(64usize, 512u32), (2048, 512), (8192, 2048), (16_384, 4096)] {
        let told = Arc::new(AtomicUsize::new(0));
        let mut node = MidiInputNode::new(Some(Box::new(Wire(Arc::clone(&told)))));
        node.prepare(&Prepare::new(SampleRate(48_000.0), Samples(block)));
        assert_eq!(
            node.shape().event_capacity.map(|n| n.get()),
            Some(want),
            "block {block}"
        );
        assert_eq!(told.load(Ordering::Relaxed), want as usize, "block {block}");
    }
}

/// The harmony node's port is sized at two events a frame: a loop of 8 frames
/// with a chord and a scale change in it, in a 1,024-frame block, is 128
/// passes of two events each, 256 events. Its port, of 2,048, holds them.
///
/// Mutation: leave the capacity at its floor of 256 plus nothing (ignore
/// `prepare`) → the port reads 256 at 1,024 frames → fails.
#[test]
fn the_harmony_port_follows_the_block() {
    use tutti_midi_runtime::HarmonyNode;
    for (block, want) in [(64usize, 256u32), (1024, 2048), (16_384, 16_384)] {
        let mut node = HarmonyNode::new([]);
        node.prepare(&Prepare::new(SampleRate(48_000.0), Samples(block)));
        assert_eq!(
            node.shape().event_capacity.map(|n| n.get()),
            Some(want),
            "block {block}"
        );
    }
}
