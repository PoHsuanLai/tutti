//! A clip loses no event it can place, and none it cannot place goes
//! unreported or leaves a note sounding.
//!
//! Every figure is counted by hand from the clip below, not from another
//! render. The clip plays at 120 BPM and 48 kHz, so a beat is 24,000 frames.

use std::sync::{Arc, Mutex};

use tutti_graph::{
    Cx, Editor, EventEdge, EventIn, EventKind, EventLoss, EventOut, Executor, Io, Node, Prepare,
    Shape, Status, Transport, Unforkable,
};
use tutti_midi_runtime::{MidiClipNode, TimedMidiEvent};
use tutti_midi_types::MidiEvent;
use tutti_types::{Beat, Bpm, NodeKey, SampleRate, Samples};
use tutti_ump::{MidiChannel, MidiGroup, Ump};

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
            tutti_types::ChannelLayout::EMPTY,
            tutti_types::ChannelLayout::EMPTY,
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
/// a block holds about 341 events, at 1,024 about 683, at 8,192 about 5,461:
/// each under that block's capacity (512, 1,024 and 8,192).
///
/// The same clip lost 25% at 512 frames, 62% at 1,024 and 94% at 8,192 with
/// the fixed 256. (A block past 8,192 gets 8,192: at 16,384 the clip's 10,923
/// events a block overflow, and that is reported;
/// `a_block_past_the_cap_overflows_and_says_so`.)
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
    for block in [64usize, 256, 512, 1024, 2048, 4096, 8192] {
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
    // Every note-off arrived: refused once, none lost.
    assert_eq!(r.exec.lost_event_count(), 0);
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
        (16_384, 8_192),
        (1 << 20, 8_192),
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
/// up to a 4,096-frame block, an eighth of the block's frames past that (1,024
/// at 8,192, 2,048 at 16,384); and its wire is told (`MidiIn::reserve`), so
/// the drain takes as many.
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

    for (block, want) in [(64usize, 512u32), (4096, 512), (8192, 1024), (16_384, 2048)] {
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
    for (block, want) in [(64usize, 256u32), (1024, 2048), (16_384, 8_192)] {
        let mut node = HarmonyNode::new([]);
        node.prepare(&Prepare::new(SampleRate(48_000.0), Samples(block)));
        assert_eq!(
            node.shape().event_capacity.map(|n| n.get()),
            Some(want),
            "block {block}"
        );
    }
}

/// A note-off refused again while owed is the same loss, and one sent after
/// is no loss. Blocks of 64 (port 256, room 296): 400 notes sound by the third
/// block, where 300 more start on frame 130 and the 400 end on frame 140.
/// Block 3: 256 note-ons land and 44 are refused; 40 note-offs land after
/// them and 360 are refused. Block 4: the retry sends 296 (the port's room)
/// and is refused on the 297th, so 64 are still owed. Block 5: they go.
/// Refused or withheld: 44 + 360 + 1 repeat = 405 (the writer counts every
/// attempt); the repeat and the 360 sent are taken back, so 44 are lost, the
/// note-ons. All 400 note-offs arrive.
///
/// Mutation: drop `out.recovered()` in the failed branch of `retry` → lost is
/// 45 → fails. Mutation: drop it after a sent note-off → lost is 404 → fails.
#[test]
fn a_note_off_refused_twice_and_sent_is_not_lost() {
    let held = (0..400).flat_map(|i| {
        let start = if i < 200 { 10 } else { 70 };
        [at(start, on(i)), at(140 + 64, off(i))]
    });
    let more = (400..700).map(|i| at(130 + 64, on(i)));
    let mut r = rig(held.chain(more), 64);
    let t = |f: u64| Transport::new(true, Bpm(120.0), beat_of(f), None);
    for k in 0..7u64 {
        r.exec.process(64, &t(64 * k), &[], &mut []);
    }
    let tally = r.tally.lock().unwrap();
    assert_eq!(tally.offs, 400, "every owed note-off arrived");
    drop(tally);
    assert_eq!(r.exec.dropped_event_count(), 405);
    assert_eq!(r.exec.lost_event_count(), 44);
}

/// Note A's note-on refused, note B taking the key and A's note-off
/// swallowed: the withheld note-off is counted, because B starting does not
/// clear A's mark. 256 other notes fill the port on frame 10 with A the
/// 257th (refused); B starts the key on frame 70 (a later block) and A ends
/// on frame 100, B on frame 200. Lost: A's note-on and A's note-off, 2.
///
/// Mutation: clear the refused mark when a note-on takes the key (the old
/// line in `HeldNotes::play`) → the count is 1 → fails.
#[test]
fn a_note_that_takes_a_refused_key_still_counts_the_old_notes_off() {
    let filler = (1..257).map(|i| at(10, on(i)));
    let a = [at(10, on(0)), at(100, off(0))];
    let b = [at(70, on(0)), at(200, off(0))];
    // Pairing is by list order: A's on then B's on, A's off then B's off.
    let events: Vec<_> = filler.chain([a[0], b[0], a[1], b[1]]).collect();
    let mut r = rig(events, 64);
    let losses = r.roll(256, 64);
    assert_eq!(r.exec.dropped_event_count(), 2, "{losses:?}");
    assert_eq!(r.exec.lost_event_count(), 2);
}

/// A mark left by a refused note-on does not survive a seek: playing on
/// from beat 0 with the note-on refused, then seeking past it, its note-off
/// is not a loss of this pass.
///
/// Mutation: skip the `refused.fill(0)` in `HeldNotes::release` → the
/// note-off after the seek is counted → fails.
#[test]
fn a_seek_forgets_what_the_port_refused() {
    let filler = (1..257).map(|i| at(10, on(i)));
    let events: Vec<_> = filler.chain([at(10, on(0)), at(10_000, off(0))]).collect();
    let mut r = rig(events, 64);
    let t = |f: u64| Transport::new(true, Bpm(120.0), beat_of(f), None);
    r.exec.process(64, &t(0), &[], &mut []);
    assert_eq!(r.exec.dropped_event_count(), 1);
    // Seek to just before the note-off: the pass starts after the note-on.
    r.exec.process(64, &t(9_950), &[], &mut []);
    r.exec.process(64, &t(10_014), &[], &mut []);
    assert_eq!(
        r.exec.dropped_event_count(),
        1,
        "the off is not this pass's loss"
    );
}

/// A block past the cap overflows and says so. At 16,384 frames the dense
/// clip (10,922 events a block) meets a port of 8,192 (`MAX_EVENT_PORT_CAPACITY`):
/// the first block's 10,922 events keep 8,192 and a note-off has room past
/// that; what is lost is counted exactly (`lost_event_count` equals the
/// events that did not arrive), never zero.
///
/// Mutation: lift the cap (`MAX_EVENT_PORT_CAPACITY` 16,384) → nothing is lost
/// → fails.
#[test]
fn a_block_past_the_cap_overflows_and_says_so() {
    let events = (0..10_000u32).flat_map(|i| {
        let f = 3 * u64::from(i);
        [at(f, on(i % 2048)), at(f + 2, off(i % 2048))]
    });
    let mut r = rig(events, 16_384);
    r.roll(30_100, 16_384);
    let t = r.tally.lock().unwrap();
    let arrived = t.ons + t.offs;
    drop(t);
    let lost = r.exec.lost_event_count();
    assert!(lost > 0, "the port is capped");
    assert_eq!(
        arrived as u64 + lost,
        20_000,
        "every event arrived or is counted"
    );
}

/// A release the port cannot hold counts every held note that got no
/// note-off, not only the first push refused. 400 notes sound (two blocks of
/// 200), the transport stops, and the release on the stop block writes 296
/// note-offs (the port's 256 and its note-off room); the other 104 are owed.
/// The block ends there: 104 held notes have no note-off, and the figure says
/// 104, not 1. Sent on the next block, they are taken back.
///
/// Mutation: stop counting at the first refused push in `release` (return
/// there, as it did) → 1 → fails.
#[test]
fn a_release_the_port_cannot_hold_counts_every_note_left_on() {
    let events = (0..400).map(|i| at(if i < 200 { 10 } else { 70 }, on(i)));
    let mut r = rig(events, 64);
    let rolling = |f: u64| Transport::new(true, Bpm(120.0), beat_of(f), None);
    for k in 0..3u64 {
        r.exec.process(64, &rolling(64 * k), &[], &mut []);
    }
    assert_eq!(r.tally.lock().unwrap().held.len(), 400);
    // Stopped: the clip releases everything it holds.
    let stopped = Transport::new(false, Bpm(120.0), beat_of(192), None);
    r.exec.process(64, &stopped, &[], &mut []);
    assert_eq!(r.tally.lock().unwrap().held.len(), 104, "296 ended");
    assert_eq!(r.exec.dropped_event_count(), 104);
    assert_eq!(r.exec.lost_event_count(), 104, "one per note left on");
    r.exec.process(64, &stopped, &[], &mut []);
    assert!(r.tally.lock().unwrap().held.is_empty());
    assert_eq!(r.exec.lost_event_count(), 0, "all of them went out");
}
