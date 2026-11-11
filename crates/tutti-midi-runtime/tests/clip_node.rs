//! [`MidiClipNode`]: a clip plays its notes out of an event port, on the
//! frames playback reaches their beats in the block's `Env` — through a seek,
//! a loop wrap, a stop and a replaced clip — and ends the notes it leaves
//! sounding when playback jumps.
//!
//! Every test renders a clip node into a sink that logs each event it
//! receives with its absolute frame. 120 BPM at 48 kHz: a beat is 24 000
//! frames, so a frame is exact in beats.

use std::sync::{Arc, Mutex};

use tutti_fixtures::midi::{cc, key_of, note_off, note_on, pitch_bend};
use tutti_graph::NodeKey;
use tutti_graph::{
    Cx, Editor, EventEdge, EventIn, EventKind, EventOut, Executor, IntoNode, Io, LoopRange, Node,
    Offset, Prepare, Shape, Status, Transport, TransportChanges, Unforkable,
};
use tutti_midi_runtime::{MidiClipControls, MidiClipNode, NoteLedger, TimedMidiEvent};
use tutti_types::{Beat, Bpm, SampleRate, Samples};
use tutti_ump::Ump;
use tutti_ump::{CCNumber, MidiChannel, MidiGroup, U7, U14, Voice1};

const RATE: f64 = 48_000.0;
const FRAMES_PER_BEAT: f64 = 24_000.0;

/// `(absolute frame, packet)` of every event the sink received.
type Seen = Arc<Mutex<Vec<(u64, Ump)>>>;

struct Sink(Seen);

impl Node for Sink {
    fn shape(&self) -> Shape {
        Shape::EMPTY.with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
        let mut seen = self.0.lock().unwrap();
        for e in io.events(0) {
            let EventKind::Midi(ump) = e.kind else {
                panic!("a clip sends MIDI")
            };
            seen.push((cx.env.frame.get() + u64::from(e.offset.get()), ump));
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

fn beat_of(frame: u64) -> Beat {
    Beat(frame as f64 / FRAMES_PER_BEAT)
}

fn on(note: u8) -> Ump {
    note_on(MidiGroup::FIRST, MidiChannel::FIRST, note, 0xFFFF)
}

fn off(note: u8) -> Ump {
    note_off(MidiGroup::FIRST, MidiChannel::FIRST, note, 0)
}

fn at(frame: u64, event: Ump) -> TimedMidiEvent {
    TimedMidiEvent::new(beat_of(frame), event)
}

/// A clip at key 1 feeding the sink at key 2, blocks of at most `max`.
fn rig(clip: MidiClipNode, max: usize) -> (Editor, Executor, MidiClipControls, Seen) {
    rig_with(clip, max)
}

fn rig_with<N: IntoNode>(clip: N, max: usize) -> (Editor, Executor, N::Controls, Seen) {
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(RATE), Samples(max)));
    let controls = ed.insert(NodeKey(1), "clip", clip);
    let seen = Seen::default();
    ed.insert(NodeKey(2), "sink", Unforkable(Sink(Arc::clone(&seen))));
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
    (ed, exec, controls, seen)
}

/// Rolling at 120 BPM from beat `frame / 24 000`.
fn rolling(frame: u64) -> Transport {
    Transport::new(true, Bpm::of(120.0), beat_of(frame), None)
}

/// Render `frames` in blocks of `block`, rolling from frame 0.
fn roll(exec: &mut Executor, frames: u64, block: usize) {
    let mut f = 0;
    while f < frames {
        exec.process(block, &rolling(f), &[], &mut []);
        f += block as u64;
    }
}

/// **Notes land on their frames, whatever the blocks.** Onsets inside a
/// block, on a block's first and last frames, and a note-off, each at its
/// absolute frame, rendered in 256- and 100-frame blocks.
///
/// Mutation: write every event at offset 0 (ignore `due`'s offset) → the
/// frames collapse onto block starts → fails. Mutation: read only the
/// block's own transport, not its segments (`segment` over `cx.env` whole) →
/// no change here (no changes in these blocks), covered by the seek test.
#[test]
fn notes_land_on_their_frames_whatever_the_blocks() {
    let frames = [100u64, 255, 256, 999, 1_000, 5_000];
    for block in [256usize, 100] {
        let clip = MidiClipNode::new(
            frames
                .iter()
                .enumerate()
                .map(|(i, &f)| at(f, if i % 2 == 0 { on(60) } else { off(60) })),
        );
        let (_ed, mut exec, _c, seen) = rig(clip, 256);
        roll(&mut exec, 6_000, block);
        let got: Vec<u64> = seen.lock().unwrap().iter().map(|&(f, _)| f).collect();
        assert_eq!(got, frames, "{block}-frame blocks");
    }
}

/// **A seek inside a block plays from the seek, on the seek's frame, and ends
/// what was sounding there.** Block 0 rolls from beat 0 and seeks at frame
/// 200 to frame 48 000. The note-on at frame 24 plays; the one at 48 100
/// plays at block frame 300; the held note 60 ends at 200, before it.
///
/// Mutation: place by the block's own transport, not per segment → the
/// post-seek note is not found (it is two beats ahead) → fails. Mutation:
/// skip the release on a jump → no note-off at 200 → fails.
#[test]
fn a_seek_inside_a_block_plays_from_the_seek() {
    let clip = MidiClipNode::new([at(24, on(60)), at(48_100, on(62))]);
    let (_ed, mut exec, _c, seen) = rig(clip, 512);
    let mut changes = TransportChanges::NONE;
    changes
        .push(Offset::new(200, Samples(512)).unwrap(), rolling(48_000))
        .unwrap();
    exec.process_with_changes(512, &rolling(0), &changes, &[], &mut []);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![(24, on(60)), (200, off(60)), (300, on(62))]
    );
}

/// **A loop that wraps inside a block plays the loop's start after the wrap,
/// and ends the notes held across it.** The loop is beat [0, 1) (24 000
/// timeline frames). A 100-frame block from timeline frame 23 800 plays note
/// 60 (at 23 850); the next block, from 23 900, plays note 64 at its frame 50,
/// wraps at its frame 100, ends both there, and plays the note at loop frame
/// 48 at its frame 148. (The sink logs the executor's frames, which start at
/// 0: timeline frame 23 800 is executor frame 0.)
///
/// Mutation: drop the post-wrap range → the note at 148 is missing → fails.
/// Mutation: skip the release at the wrap → no note-off at 100 → fails.
#[test]
fn a_loop_that_wraps_inside_a_block_plays_its_start() {
    let clip = MidiClipNode::new([at(48, on(62)), at(23_850, on(60)), at(23_950, on(64))]);
    let (_ed, mut exec, _c, seen) = rig(clip, 512);
    let lp = Some(LoopRange {
        start: Beat(0.0),
        end: Beat(1.0),
    });
    let looping = |f: u64| Transport::new(true, Bpm::of(120.0), beat_of(f), lp);
    exec.process(100, &looping(23_800), &[], &mut []);
    exec.process(512, &looping(23_900), &[], &mut []);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            (50, on(60)),
            (150, on(64)),
            (200, off(60)),
            (200, off(64)),
            (248, on(62)),
        ]
    );
}

/// **Stopping ends every held note, once.** A note-on at frame 10, then a
/// stopped block: its note-off is on the stopped block's first frame, and a
/// second stopped block sends nothing.
///
/// Mutation: skip the release while stopped → no note-off → fails.
#[test]
fn stopping_ends_the_held_notes_once() {
    let clip = MidiClipNode::new([at(10, on(60))]);
    let (_ed, mut exec, _c, seen) = rig(clip, 64);
    exec.process(64, &rolling(0), &[], &mut []);
    let stopped = Transport::new(false, Bpm::of(120.0), beat_of(64), None);
    exec.process(64, &stopped, &[], &mut []);
    exec.process(64, &stopped, &[], &mut []);
    assert_eq!(*seen.lock().unwrap(), vec![(10, on(60)), (64, off(60))]);
}

/// **A new clip ends the old clip's notes, and plays from where playback
/// is.** Note 60 sounds from frame 10; the clip is replaced before the
/// second block, whose first frame gets the note-off, and the new clip's note
/// at frame 100 plays.
///
/// Mutation: ignore the generation change → note 60 is never ended → fails.
#[test]
fn a_new_clip_ends_the_old_clips_notes() {
    let clip = MidiClipNode::new([at(10, on(60)), at(1_000, off(60))]);
    let (_ed, mut exec, controls, seen) = rig(clip, 64);
    exec.process(64, &rolling(0), &[], &mut []);
    controls.set_events([at(100, on(72))]);
    assert_eq!(controls.event_count(), 1);
    exec.process(64, &rolling(64), &[], &mut []);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![(10, on(60)), (64, off(60)), (100, on(72))]
    );
}

/// **A fork plays the clip as it was when forked.** The live clip is
/// replaced after the fork; the forked node still plays the original's note,
/// at its frame, on the `Env` it is rendered with.
///
/// Mutation: fork over the live cell rather than a snapshot of the clip →
/// the fork plays the replacement → fails.
#[test]
fn a_fork_plays_the_clip_as_it_was_at_the_fork() {
    use tutti_graph::{ForkMode, NodeParts};
    let NodeParts { controls, fork, .. } = MidiClipNode::new([at(300, on(60))]).into_parts();
    let forked = fork
        .expect("a clip is forkable")
        .fork(ForkMode::Live)
        .expect("a clip forks");
    controls.set_events([at(100, on(72))]);
    let (_ed, mut exec, (), seen) = rig_with(Unforkable(forked.node), 256);
    roll(&mut exec, 512, 256);
    assert_eq!(*seen.lock().unwrap(), vec![(300, on(60))]);
}

/// **A loop that wraps exactly at a block's end ends the notes held across
/// it**, at the next block's first frame. Loop [0, 1); 100-frame blocks from
/// timeline frames 23 800 and 23 900 (which ends on the loop's end), then
/// from 0. Note 60, on at 23 850, is ended at executor frame 200.
///
/// Mutation: keep `expected` after an edge wrap → the next block reads as
/// continuous and the note is never ended → fails.
#[test]
fn a_loop_that_wraps_at_a_block_edge_ends_its_held_notes() {
    let clip = MidiClipNode::new([at(23_850, on(60))]);
    let (_ed, mut exec, _c, seen) = rig(clip, 512);
    let lp = Some(LoopRange {
        start: Beat(0.0),
        end: Beat(1.0),
    });
    let looping = |f: u64| Transport::new(true, Bpm::of(120.0), beat_of(f), lp);
    exec.process(100, &looping(23_800), &[], &mut []);
    exec.process(100, &looping(23_900), &[], &mut []);
    exec.process(100, &looping(0), &[], &mut []);
    assert_eq!(*seen.lock().unwrap(), vec![(50, on(60)), (200, off(60))]);
}

/// **A beat that is not finite is dropped, not the clip.** A NaN made at run
/// time (negative on x86, which `total_cmp` sorts first) beside a note at
/// frame 100: the note plays.
///
/// Mutation: keep non-finite beats in `sorted` → the NaN sorts first, the
/// walk stops on it, nothing plays → fails.
#[test]
fn a_nan_beat_does_not_silence_the_clip() {
    let nan = Beat(0.0 / std::hint::black_box(0.0));
    let clip = MidiClipNode::new([TimedMidiEvent::new(nan, on(61)), at(100, on(60))]);
    let (_ed, mut exec, _c, seen) = rig(clip, 256);
    roll(&mut exec, 256, 256);
    assert_eq!(*seen.lock().unwrap(), vec![(100, on(60))]);
}

/// **Note-offs the port refuses go out in the blocks after, until none is
/// held.** 400 notes are started over two blocks (200 each, under the
/// port's 256); a seek then owes 400 note-offs: 296 on the seek block's
/// first frame (the port's 256 and its note-off room, `note_off_room`), the
/// other 104 on the next block's, which rolls on from the seek (continuous:
/// nothing but the debt would release them).
///
/// Mutation: never retry an owed release → 144 notes hang → fails.
#[test]
fn refused_note_offs_are_retried_until_none_is_held() {
    let note = |i: u16| {
        note_on(
            MidiGroup::FIRST,
            MidiChannel::of((i / 128) as u8),
            (i % 128) as u8,
            0xFFFF,
        )
    };
    let clip = MidiClipNode::new((0..400u16).map(|i| at(if i < 200 { 10 } else { 300 }, note(i))));
    let (_ed, mut exec, _c, seen) = rig(clip, 256);
    roll(&mut exec, 512, 256);
    // The seek, far from any event, then rolling on from it.
    for f in [48_000u64, 48_256, 48_512] {
        exec.process(256, &rolling(f), &[], &mut []);
    }
    let seen = seen.lock().unwrap();
    let offs: Vec<u64> = seen
        .iter()
        .filter(|(_, ump)| key_of(ump).is_some_and(|(_, _, on)| !on))
        .map(|&(f, _)| f)
        .collect();
    assert_eq!(offs.len(), 400, "every started note is ended");
    let room = tutti_graph::note_off_room(256);
    assert_eq!(room, 296);
    assert_eq!(offs.iter().filter(|&&f| f == 512).count(), room);
    assert_eq!(offs.iter().filter(|&&f| f == 768).count(), 400 - room);
}

/// **Setting the events the clip already holds cuts nothing.** Note 60 is
/// sounding; the same events (in another order) are set again; no note-off.
///
/// Mutation: publish a new generation whatever the events → note 60 is ended
/// at 64 → fails.
#[test]
fn setting_the_same_events_cuts_nothing() {
    let events = [at(10, on(60)), at(1_000, off(60))];
    let (_ed, mut exec, controls, seen) = rig(MidiClipNode::new(events), 64);
    exec.process(64, &rolling(0), &[], &mut []);
    controls.set_events([events[1], events[0]]);
    exec.process(64, &rolling(64), &[], &mut []);
    assert_eq!(*seen.lock().unwrap(), vec![(10, on(60))]);
}

/// **A note lands on its frame where the beat is not a whole number of
/// frames' worth of float** (the frame, not the beat, is the source of
/// truth). At 90 BPM and 48 kHz a beat is 32 000 frames: notes at beats 1
/// and 3 land on frames 32 000 and 96 000 exactly, in 64-frame blocks whose
/// transport beat is `frame / 32 000` (not exact in binary).
///
/// Carried over from the retired clip-source test, which caught a clip
/// flooring a beat's offset onto the frame before it.
///
/// Mutation (run): place by `((beat - now) * fpb) as usize` in the walk
/// instead of `Env::due` → beat 3 lands on 95 999 → fails.
#[test]
fn a_note_lands_on_its_frame_at_ninety_bpm() {
    let clip = MidiClipNode::new([
        TimedMidiEvent::new(Beat(1.0), on(60)),
        TimedMidiEvent::new(Beat(3.0), on(62)),
    ]);
    let (_ed, mut exec, _c, seen) = rig(clip, 64);
    let mut f = 0u64;
    while f < 100_000 {
        let t = Transport::new(true, Bpm::of(90.0), Beat(f as f64 / 32_000.0), None);
        exec.process(64, &t, &[], &mut []);
        f += 64;
    }
    assert_eq!(
        *seen.lock().unwrap(),
        vec![(32_000, on(60)), (96_000, on(62))]
    );
}

/// **A loop shorter than a block plays every pass the block holds.** The
/// loop is 100 frames (beats [0, 100 / 24 000)); note 60 is on at loop frame
/// 10 and off at 60. Two 512-frame blocks, the second from where the first
/// left the playhead (loop frame 12): the note plays on every pass, eleven
/// times, and is ended within each.
///
/// Mutation (run): walk only the first wrap of a segment (the pass loop
/// broken after one iteration) → the first block plays passes 0 and 1 only →
/// fails. Mutation (run): the continuity check's `next` wrapped once rather
/// than modulo the loop → the second block reads as a jump and the note held
/// across the block edge is ended at its first frame → fails.
#[test]
fn a_loop_shorter_than_a_block_plays_every_pass() {
    let clip = MidiClipNode::new([at(10, on(60)), at(60, off(60))]);
    let (_ed, mut exec, _c, seen) = rig(clip, 512);
    let lp = Some(LoopRange {
        start: Beat(0.0),
        end: beat_of(100),
    });
    let looping = |f: u64| Transport::new(true, Bpm::of(120.0), beat_of(f), lp);
    exec.process(512, &looping(0), &[], &mut []);
    exec.process(512, &looping(12), &[], &mut []);
    let mut want: Vec<(u64, Ump)> = (0..11)
        .map(|k| (10 + 100 * k, on(60)))
        .chain((0..10).map(|k| (60 + 100 * k, off(60))))
        .collect();
    want.sort_unstable_by_key(|&(f, u)| (f, u.0));
    assert_eq!(*seen.lock().unwrap(), want);
}

/// **A note started twice and ended once leaves nothing sounding.** A key is
/// held or not: the second note-on of note 60 (frame 20, while it holds) is
/// preceded by a note-off on its frame, so a receiver that stacks a voice per
/// note-on still has one. The one note-off (frame 30) is the first note's,
/// already ended, and is swallowed: the second note sounds on until the seek
/// at frame 64 ends it.
///
/// Mutation (run): the clip's event visit writing each event as it is (no
/// `Sending::send`) → the stream is on, on, off: a stacking receiver keeps
/// a voice the clip no longer holds, and the seek ends nothing → fails.
#[test]
fn a_note_started_twice_and_ended_once_leaves_nothing_sounding() {
    let clip = MidiClipNode::new([at(10, on(60)), at(20, on(60)), at(30, off(60))]);
    let (_ed, mut exec, _c, seen) = rig(clip, 64);
    exec.process(64, &rolling(0), &[], &mut []);
    exec.process(64, &rolling(48_000), &[], &mut []);
    let seen = seen.lock().unwrap();
    assert_eq!(
        *seen,
        vec![(10, on(60)), (20, off(60)), (20, on(60)), (64, off(60)),]
    );
    // A receiver that starts a voice per note-on and ends one per note-off.
    let voices = seen.iter().fold(0i32, |v, (_, ump)| match key_of(ump) {
        Some((_, _, true)) => v + 1,
        Some((_, _, false)) => (v - 1).max(0),
        None => v,
    });
    assert_eq!(voices, 0, "nothing left sounding");
}

/// **Overlapping notes of one key: the newer keeps its full length.** A is
/// on 0..100, B (the same key) on 50..150. The stream is A on at 0, A ended
/// and B on at 50, and B's note-off at 150: nothing at 100 (A's own
/// note-off, swallowed), so B is not cut there and no stray note-off
/// follows it.
///
/// Mutation (run): a note-off pairs with the latest-listed note-on of its
/// key (the pairing scan in `sorted` reversed) → the note-off at 100 is B's
/// and cuts it → fails.
/// Mutation (run): no note-off before the restart → B's note-on stacks on A
/// → fails.
#[test]
fn overlapping_notes_of_one_key_keep_the_newer_whole() {
    let clip = MidiClipNode::new([
        at(0, on(60)),
        at(50, on(60)),
        at(100, off(60)),
        at(150, off(60)),
    ]);
    let (_ed, mut exec, _c, seen) = rig(clip, 256);
    exec.process(256, &rolling(0), &[], &mut []);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![(0, on(60)), (50, off(60)), (50, on(60)), (150, off(60)),]
    );
}

/// **A note nested inside a longer one of the same key ends at its own
/// end.** A is on 0..100, B on 20..50, each listed with its note-off. B's
/// note-on ends A on its frame; B's note-off at 50 ends the key; A's own
/// note-off at 100 is swallowed, so nothing follows.
///
/// Mutation (run): the pairing dropped (every note-off of no note, written
/// as it comes) → a stray note-off at 100 → fails. Mutation (run): the
/// pairing scan in `sorted` reversed (latest-listed note-on first) → A's
/// note-off is paired to B and B's to A: B's note-off at 50 is swallowed and
/// B sounds to 100 → fails.
#[test]
fn a_note_nested_in_another_of_its_key_ends_at_its_own_end() {
    let clip = MidiClipNode::new([
        at(0, on(60)),
        at(100, off(60)),
        at(20, on(60)),
        at(50, off(60)),
    ]);
    let (_ed, mut exec, _c, seen) = rig(clip, 256);
    exec.process(256, &rolling(0), &[], &mut []);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![(0, on(60)), (20, off(60)), (20, on(60)), (50, off(60)),]
    );
}

/// **Three overlapping notes of one key: the newest sounds to its end, and
/// no stray note-off follows.** A on 0..30, B on 10..40, C on 20..50. Each
/// note-on ends the one sounding; A's and B's note-offs (30, 40) are
/// swallowed, C's ends the key at 50.
///
/// Mutation (run): a note-off ends its key whoever owns it (the
/// `e.owner == owner` check dropped) → A's note-off cuts C at 30 → fails.
/// Mutation (run): a restart not taking the key (`owner` stored in the entry dropped
/// from the held branch of `Sending::on`) → A still owns the key, and
/// its note-off cuts C at 30 → fails.
#[test]
fn three_overlapping_notes_of_one_key_leave_the_newest_whole() {
    let clip = MidiClipNode::new([
        at(0, on(60)),
        at(30, off(60)),
        at(10, on(60)),
        at(40, off(60)),
        at(20, on(60)),
        at(50, off(60)),
    ]);
    let (_ed, mut exec, _c, seen) = rig(clip, 256);
    exec.process(256, &rolling(0), &[], &mut []);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            (0, on(60)),
            (10, off(60)),
            (10, on(60)),
            (20, off(60)),
            (20, on(60)),
            (50, off(60)),
        ]
    );
}

/// **A key's owner starts over at a loop wrap.** A 100-frame loop, six
/// passes: A on
/// 10..30, B on 50 and off past the loop's end, so B holds the key at every
/// wrap and the wrap ends it. Each pass then plays A whole: A's note-on owns
/// the key again, and its note-off at 30 ends it.
///
/// Mutation (run): a note-on of an unheld key not taking it → A's note-off
/// is swallowed, and A sounds to 50 → fails. Mutation (run): that note-on
/// taking the key only when it has no owner, with `release` leaving the
/// owner set → the first pass is right, but after the wrap the owner is
/// still B and A sounds on to B's note-on → fails.
#[test]
fn a_keys_owner_starts_over_at_a_loop_wrap() {
    let clip = MidiClipNode::new([
        at(10, on(60)),
        at(30, off(60)),
        at(50, on(60)),
        at(150, off(60)),
    ]);
    let (_ed, mut exec, _c, seen) = rig(clip, 512);
    // Three passes, then a block from the loop's start (the third wrap falls
    // on the first block's edge, and is ended on the next block's first frame).
    for _ in 0..2 {
        exec.process(
            300,
            &looping_at(120.0, Beat(0.0), Beat(0.0), beat_of(100)),
            &[],
            &mut [],
        );
    }
    let want: Vec<(u64, Ump)> = (0..6)
        .flat_map(|k| {
            let p = 100 * k;
            [
                (p + 10, on(60)),
                (p + 30, off(60)),
                (p + 50, on(60)),
                (p + 100, off(60)),
            ]
        })
        .filter(|&(f, _)| f < 600)
        .collect();
    assert_eq!(*seen.lock().unwrap(), want);
}

/// A loop of `[start, end)` beats under `tempo`, rolling from `beat`.
fn looping_at(tempo: f64, beat: Beat, start: Beat, end: Beat) -> Transport {
    Transport::new(true, Bpm::of(tempo), beat, Some(LoopRange { start, end }))
}

/// **A pass that ends exactly on a block's edge ends what it held at the
/// next block's first frame.** A 100-frame loop, 500-frame blocks: note 60 on
/// at loop frame 95 and never off, so every wrap ends it. The fifth wrap of
/// the first block falls on its end (frame 500); the next block starts on
/// the loop's start, which reads as continuous, and still ends the note on
/// its first frame.
///
/// Mutation (run): `wraps_at_end` left false for a pass after the first →
/// the note held across frame 500 is not ended there → fails.
#[test]
fn a_pass_ending_on_a_block_edge_ends_what_it_held() {
    let clip = MidiClipNode::new([at(95, on(60))]);
    let (_ed, mut exec, _c, seen) = rig(clip, 512);
    exec.process(
        500,
        &looping_at(120.0, Beat(0.0), Beat(0.0), beat_of(100)),
        &[],
        &mut [],
    );
    exec.process(
        500,
        &looping_at(120.0, Beat(0.0), Beat(0.0), beat_of(100)),
        &[],
        &mut [],
    );
    let offs: Vec<u64> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|&&(_, w)| w == off(60))
        .map(|&(f, _)| f)
        .collect();
    assert_eq!(offs, (1..10).map(|k| 100 * k).collect::<Vec<u64>>());
}

/// **A tempo change inside a block re-times the passes after it.** A
/// 100-frame loop at 120 BPM (beats [0, 100 / 24 000)); note 60 on at loop
/// frame 10, off at 60. At frame 250 (loop frame 50) the tempo doubles, so a
/// pass is 50 frames: the note-off still owed in that pass lands 5 frames
/// on (255), the loop wraps at 275, and each 50-frame pass after plays the
/// note 5 frames in and ends it 30 frames in.
///
/// Mutation (run): every segment walked with the block's first transport
/// (`block.transport` for `seg.transport` in `Walk::segment`) → the passes
/// after the change keep the old tempo's length → fails.
#[test]
fn a_tempo_change_inside_a_block_re_times_the_passes() {
    let clip = MidiClipNode::new([at(10, on(60)), at(60, off(60))]);
    let (_ed, mut exec, _c, seen) = rig(clip, 512);
    let mut changes = TransportChanges::NONE;
    changes
        .push(
            Offset::new(250, Samples(512)).unwrap(),
            looping_at(240.0, beat_of(50), Beat(0.0), beat_of(100)),
        )
        .unwrap();
    exec.process_with_changes(
        512,
        &looping_at(120.0, Beat(0.0), Beat(0.0), beat_of(100)),
        &changes,
        &[],
        &mut [],
    );
    let seen = seen.lock().unwrap();
    let frames = |w: Ump| -> Vec<u64> {
        seen.iter()
            .filter(|&&(_, x)| x == w)
            .map(|&(f, _)| f)
            .collect()
    };
    assert_eq!(frames(on(60)), vec![10, 110, 210, 280, 330, 380, 430, 480]);
    assert_eq!(frames(off(60)), vec![60, 160, 255, 305, 355, 405, 455, 505]);
}

/// **Every pass lands where the host's clock reaches its beats**, at a tempo
/// and a loop that are no whole number of frames (97 BPM at 48 kHz, a loop
/// of 0.00126 beat ≈ 37.4 frames), over four 512-frame blocks, each started
/// where the clock stands. The reference is `FrameClock` walked frame by
/// frame (an analytic walk, not a render): a note fires on the first frame
/// whose beat reaches it, and on a wrap's frame for a beat the wrap
/// crossed.
///
/// Mutation (run): pass events placed from the loop's start rather than the
/// beat the clock wrapped to (`from` = `l.start()`) → notes land a frame
/// early where a wrap overshoots → fails.
#[test]
fn every_pass_lands_where_the_hosts_clock_reaches_it() {
    use tutti_types::{FRAME_TOLERANCE, FrameClock, LoopRange as Region};
    const TEMPO: f64 = 97.0;
    let (start, end) = (Beat(3.0), Beat(3.00126));
    let fpb = RATE * 60.0 / TEMPO;
    let notes = [3.000_1, 3.000_5, 3.001_1];
    let clip = MidiClipNode::new(
        notes
            .iter()
            .enumerate()
            .map(|(i, &b)| TimedMidiEvent::new(Beat(b), on(60 + i as u8))),
    );
    let (_ed, mut exec, _c, seen) = rig(clip, 512);
    let region = Region::new(start, end).unwrap();
    let mut clock = FrameClock::new(Beat(2.999), tutti_types::Bpm::of(TEMPO), SampleRate(RATE));
    for _ in 0..4 {
        exec.process(
            512,
            &looping_at(TEMPO, clock.beat(), start, end),
            &[],
            &mut [],
        );
        clock.advance(Samples(512), Some(region));
    }

    // The reference walk.
    let tol = FRAME_TOLERANCE / fpb;
    let mut want = Vec::new();
    let mut walk = FrameClock::new(Beat(2.999), tutti_types::Bpm::of(TEMPO), SampleRate(RATE));
    let mut prev: Option<f64> = None;
    for f in 0..4 * 512u64 {
        let b = walk.beat().get();
        let wrapped = prev.is_some_and(|p| b < p);
        for (i, &n) in notes.iter().enumerate() {
            let reached = b + tol >= n;
            let fresh = match prev {
                None => true,
                Some(_) if wrapped => n >= start.get(),
                Some(p) => p + tol < n,
            };
            if reached && fresh {
                want.push((f, on(60 + i as u8)));
            }
        }
        prev = Some(b);
        walk.advance(Samples(1), Some(region));
    }
    assert!(
        want.len() > 100,
        "the loop passes many times: {}",
        want.len()
    );
    // The clip also ends what it held at each wrap; the reference counts
    // only where each pass's note-ons land.
    let ons: Vec<Ump> = (0..notes.len()).map(|i| on(60 + i as u8)).collect();
    let mut got: Vec<_> = seen
        .lock()
        .unwrap()
        .iter()
        .copied()
        .filter(|(_, w)| ons.contains(w))
        .collect();
    got.sort_unstable_by_key(|&(f, u)| (f, u.0));
    want.sort_unstable_by_key(|&(f, u)| (f, u.0));
    assert_eq!(got, want);
}

/// Renders four 64-frame blocks of a clip with one note at the loop's start,
/// rolling from it, on a thread, and returns what the sink saw, or `None`
/// when that takes longer than a generous deadline.
fn sub_frame_loop(start: Beat, end: Beat) -> Option<Vec<(u64, Ump)>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let clip = MidiClipNode::new([TimedMidiEvent::new(start, on(60))]);
        let (_ed, mut exec, _c, seen) = rig(clip, 64);
        let mut clock =
            tutti_types::FrameClock::new(start, tutti_types::Bpm::of(120.0), SampleRate(RATE));
        let region = tutti_types::LoopRange::new(start, end).unwrap();
        for _ in 0..4 {
            exec.process(
                64,
                &looping_at(120.0, clock.beat(), start, end),
                &[],
                &mut [],
            );
            clock.advance(Samples(64), Some(region));
        }
        let _ = tx.send(seen.lock().unwrap().clone());
    });
    rx.recv_timeout(std::time::Duration::from_secs(10)).ok()
}

/// **A loop shorter than a frame costs one pass a frame.** A one-ulp loop
/// at beat 4 (`4.0.next_up()`, a realistic host input) and a loop of 1e-300
/// beat at 0: each frame wraps once, so the note at the loop's start plays
/// on every frame (ended on the next), and four 64-frame blocks finish at
/// once.
///
/// Mutation (run): the passes found by arithmetic on the loop's span (the
/// walk before it followed the clock, which counted passes from an
/// estimate that saturated near beat 0) → the 1e-300 loop never finishes
/// (deadline) → fails. Mutation (run): the executor's `Playhead`
/// unwrapping the next block's start one loop period at a time → the
/// one-ulp loop never finishes → fails.
#[cfg_attr(miri, ignore = "a wall-clock deadline")]
#[test]
fn a_loop_shorter_than_a_frame_costs_one_pass_a_frame() {
    for (start, end) in [
        (Beat(4.0), Beat(4.0f64.next_up())),
        (Beat(0.0), Beat(1e-300)),
    ] {
        let seen = sub_frame_loop(start, end)
            .unwrap_or_else(|| panic!("[{start:?}, {end:?}) did not finish"));
        let ons = seen.iter().filter(|&&(_, w)| w == on(60)).count();
        let offs = seen.iter().filter(|&&(_, w)| w == off(60)).count();
        assert_eq!((ons, offs), (256, 255), "[{start:?}, {end:?})");
    }
}

// ---------------------------------------------------------------------------
// Controllers back to their normal positions when playback stops or jumps.
// ---------------------------------------------------------------------------

const DOWN: u32 = 0xFFFF_FFFF;
const CENTRE: u32 = 0x8000_0000;

fn pedal(channel: u8, controller: CCNumber, value: u32) -> Ump {
    cc(
        MidiGroup::FIRST,
        MidiChannel::of(channel),
        controller,
        value,
    )
}

fn bend(channel: u8, value: u32) -> Ump {
    pitch_bend(MidiGroup::FIRST, MidiChannel::of(channel), value)
}

/// **A stop returns every pedal and the bend to their normal positions,
/// after the note-offs, once.** MIDI 1.0 Detailed Spec p. 31 (Stop): "any
/// controllers not in their initialized position (pitch wheels, sustain
/// pedal, etc.) should be returned to their normal positions"; A-5: a pedal
/// down "takes priority over Note Off and All Notes Off until it is
/// released", so the offs come first and the pedals are lifted after them.
/// Sustain, sostenuto, soft and hold 2 are down and the bend is off centre;
/// a second stopped block sends nothing.
///
/// Mutation: `release_all` without `lift_all` (the controllers table) → no
/// pedal-up and no centred bend → fails. Mutation: `lift_all` before the
/// notes are ended → the pedal-ups precede the note-off → fails.
#[test]
fn stopping_lifts_the_pedals_and_centres_the_bend_after_the_note_offs() {
    let sustain = CCNumber::SUSTAIN;
    let sostenuto = CCNumber::SOSTENUTO;
    let soft = CCNumber::SOFT_PEDAL;
    let hold2 = CCNumber::new(69).unwrap();
    let clip = MidiClipNode::new([
        at(10, on(60)),
        at(20, pedal(0, sustain, DOWN)),
        at(21, pedal(0, sostenuto, DOWN)),
        at(22, pedal(0, soft, DOWN)),
        at(23, pedal(0, hold2, DOWN)),
        at(24, bend(0, 0x9000_0000)),
    ]);
    let (_ed, mut exec, _c, seen) = rig(clip, 64);
    exec.process(64, &rolling(0), &[], &mut []);
    let stopped = Transport::new(false, Bpm::of(120.0), beat_of(64), None);
    exec.process(64, &stopped, &[], &mut []);
    exec.process(64, &stopped, &[], &mut []);
    let seen = seen.lock().unwrap();
    let after: Vec<(u64, Ump)> = seen.iter().copied().filter(|&(f, _)| f == 64).collect();
    assert_eq!(
        after,
        vec![
            (64, off(60)),
            (64, pedal(0, sustain, 0)),
            (64, pedal(0, sostenuto, 0)),
            (64, pedal(0, soft, 0)),
            (64, pedal(0, hold2, 0)),
            (64, bend(0, CENTRE)),
        ]
    );
    assert_eq!(seen.len(), 6 + 6, "nothing after the first stopped block");
}

/// **A controller already at its normal position is not sent again.**
/// Channel 0's sustain goes down and up and its bend goes off centre and
/// back; channel 1's sustain stays down. A stop sends one pedal-up, for
/// channel 1, and nothing else (M1 p. 31 asks only for controllers "not in
/// their initialized position").
///
/// Mutation: a pedal-up for a lane whose state bit is clear (skip the
/// `held` test in `lift_all`) → channel 0's pedal and bend are resent →
/// fails. Mutation: `observe` ignoring a value that returns to rest → the
/// same.
#[test]
fn a_controller_at_rest_is_not_resent_at_a_stop() {
    let s = CCNumber::SUSTAIN;
    let clip = MidiClipNode::new([
        at(10, pedal(0, s, DOWN)),
        at(11, pedal(0, s, 0)),
        at(12, bend(0, 0xA000_0000)),
        at(13, bend(0, CENTRE)),
        at(14, pedal(1, s, DOWN)),
    ]);
    let (_ed, mut exec, _c, seen) = rig(clip, 64);
    exec.process(64, &rolling(0), &[], &mut []);
    exec.process(
        64,
        &Transport::new(false, Bpm::of(120.0), beat_of(64), None),
        &[],
        &mut [],
    );
    let late: Vec<(u64, Ump)> = seen
        .lock()
        .unwrap()
        .iter()
        .copied()
        .filter(|&(f, _)| f >= 64)
        .collect();
    assert_eq!(late, vec![(64, pedal(1, s, 0))]);
}

/// **A loop wrap lifts the pedal after ending the notes it held.** The loop
/// is beat [0, 1): note 60 and the sustain pedal go down at timeline frame
/// 23 850; the block that wraps at its frame 100 (executor frame 200) ends the
/// note, then lifts the pedal, on that frame (M1 p. 31; A-5 for the order).
///
/// Mutation: skip the controllers in the wrap's release → the pedal stays
/// down across the loop → fails.
#[test]
fn a_loop_wrap_lifts_the_pedal_after_the_note_off() {
    let clip = MidiClipNode::new([
        at(23_850, on(60)),
        at(23_851, pedal(0, CCNumber::SUSTAIN, DOWN)),
    ]);
    let (_ed, mut exec, _c, seen) = rig(clip, 512);
    let lp = Some(LoopRange {
        start: Beat(0.0),
        end: Beat(1.0),
    });
    let looping = |f: u64| Transport::new(true, Bpm::of(120.0), beat_of(f), lp);
    exec.process(100, &looping(23_800), &[], &mut []);
    exec.process(512, &looping(23_900), &[], &mut []);
    let tail: Vec<(u64, Ump)> = seen.lock().unwrap().iter().copied().skip(2).collect();
    assert_eq!(
        tail,
        vec![(200, off(60)), (200, pedal(0, CCNumber::SUSTAIN, 0))]
    );
}

/// **A seek lifts the pedal and centres the bend where the jump happens.**
/// Playback seeks to beat 2 at frame 200 of a 512-frame block, with the
/// pedal down and the bend off centre: both are returned at frame 200, after
/// the note-off, and not again by the next block.
///
/// Mutation: `Playing::jump` ending the notes without the controllers (an
/// older `release_all`) → the pedal stays down across the seek → fails.
#[test]
fn a_seek_returns_the_controllers_where_the_jump_happens() {
    let clip = MidiClipNode::new([
        at(24, on(60)),
        at(25, pedal(0, CCNumber::SUSTAIN, DOWN)),
        at(26, bend(0, 0x7000_0000)),
    ]);
    let (_ed, mut exec, _c, seen) = rig(clip, 512);
    let mut changes = TransportChanges::NONE;
    changes
        .push(Offset::new(200, Samples(512)).unwrap(), rolling(96_000))
        .unwrap();
    exec.process_with_changes(512, &rolling(0), &changes, &[], &mut []);
    exec.process(512, &rolling(96_512), &[], &mut []);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            (24, on(60)),
            (25, pedal(0, CCNumber::SUSTAIN, DOWN)),
            (26, bend(0, 0x7000_0000)),
            (200, off(60)),
            (200, pedal(0, CCNumber::SUSTAIN, 0)),
            (200, bend(0, CENTRE)),
        ]
    );
}

/// **A pedal-up the port refuses is owed and goes out after the note-offs.**
/// 400 notes and a sustain pedal are down when a seek ends them into a port
/// with room for 296 offs: the offs the port refuses are paid in the blocks
/// after, and the pedal-up, refused behind them, goes out only after the last
/// of them (A-5: a pedal still down takes priority over a note-off), exactly
/// once.
///
/// Mutation: a refused pedal-up dropped instead of owed (do not set the owed
/// bit in `lift_all`) → no pedal-up ever → fails.
#[test]
fn a_refused_pedal_up_is_owed_and_follows_the_last_note_off() {
    let note = |i: u16| {
        note_on(
            MidiGroup::FIRST,
            MidiChannel::of((i / 128) as u8),
            (i % 128) as u8,
            0xFFFF,
        )
    };
    let s = CCNumber::SUSTAIN;
    let clip = MidiClipNode::new(
        (0..400u16)
            .map(|i| at(if i < 200 { 10 } else { 300 }, note(i)))
            .chain([at(5, pedal(15, s, DOWN))]),
    );
    let (_ed, mut exec, _c, seen) = rig(clip, 256);
    roll(&mut exec, 512, 256);
    for f in [48_000u64, 48_256, 48_512] {
        exec.process(256, &rolling(f), &[], &mut []);
    }
    let seen = seen.lock().unwrap();
    let last_off = seen
        .iter()
        .rposition(|(_, u)| key_of(u).is_some_and(|(_, _, on)| !on))
        .unwrap();
    let ups: Vec<usize> = seen
        .iter()
        .enumerate()
        .filter(|&(_, &(_, u))| u == pedal(15, s, 0))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(ups.len(), 1, "the pedal is lifted exactly once");
    assert!(ups[0] > last_off, "after every note-off");
}

/// Block 0 sends `script` through a ledger; block 1 releases it all.
struct SendThenRelease {
    ledger: NoteLedger<()>,
    script: Vec<Ump>,
    block: u32,
}

impl Node for SendThenRelease {
    fn shape(&self) -> Shape {
        Shape::EMPTY.with_events(0, 1).with_event_capacity(64)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let first = cx.env.offset(0).unwrap();
        let mut sending = self.ledger.block(io.event_out(0), first);
        if self.block == 0 {
            for &u in &self.script {
                sending.send(first, u, None);
            }
        } else {
            sending.release_all(first);
        }
        self.block += 1;
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// **A MIDI 1.0 pedal and bend are returned in MIDI 1.0.** The ledger sends
/// the return in the protocol the message it was told about used: a MIDI 1.0
/// Control Change 64 with value 0 and a MIDI 1.0 Pitch Bend at 0x2000, never
/// a MIDI 2.0 packet (M1 p. 31, Stop; the 14-bit centre is 0x2000, M1 table
/// of Pitch Bend). A pedal value below 64 is up (M1 CC 64 "0-63 off"), so a
/// pedal sent at 63 is not lifted again.
///
/// Mutation: build every reset as a MIDI 2.0 packet (`m2` always true in
/// `Controllers::reset`) → the packets differ → fails.
#[test]
fn a_midi_1_pedal_and_bend_are_returned_in_midi_1() {
    let ch = MidiChannel::of(2);
    let m1_bend = |v: u16| tutti_fixtures::midi::m1(ch, Voice1::PitchBend(U14::of(v)));
    let m1_pedal = |controller, v: u8| tutti_fixtures::midi::m1_cc(ch, controller, U7::of(v));
    let script = vec![
        m1_pedal(CCNumber::SUSTAIN, 127),
        m1_pedal(CCNumber::SOSTENUTO, 63),
        m1_bend(0x3000),
    ];
    let node = SendThenRelease {
        ledger: NoteLedger::new(16),
        script: script.clone(),
        block: 0,
    };
    let (_ed, mut exec, _c, seen) = rig_with(Unforkable(node), 64);
    exec.process(64, &rolling(0), &[], &mut []);
    exec.process(64, &rolling(64), &[], &mut []);
    let late: Vec<Ump> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|&&(f, _)| f >= 64)
        .map(|&(_, u)| u)
        .collect();
    assert_eq!(late, vec![m1_pedal(CCNumber::SUSTAIN, 0), m1_bend(0x2000)]);
}
