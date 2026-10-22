//! [`MidiClipNode`]: a clip plays its notes out of an event port, on the
//! frames playback reaches their beats in the block's `Env` — through a seek,
//! a loop wrap, a stop and a replaced clip — and ends the notes it leaves
//! sounding when playback jumps.
//!
//! Every test renders a clip node into a sink that logs each event it
//! receives with its absolute frame. 120 BPM at 48 kHz: a beat is 24 000
//! frames, so a frame is exact in beats.

use std::sync::{Arc, Mutex};

use tutti_core::{Beat, Bpm, NodeKey, SampleRate, Samples};
use tutti_graph::{
    Cx, Editor, EventEdge, EventIn, EventKind, EventOut, Executor, IntoNode, Io, LoopRange, Node,
    Offset, Prepare, Shape, Status, Transport, TransportChanges, Ump, Unforkable,
};
use tutti_midi_runtime::{MidiClipControls, MidiClipNode, TimedMidiEvent};
use tutti_midi_types::{MidiChannel, MidiEvent, MidiGroup};

const RATE: f64 = 48_000.0;
const FRAMES_PER_BEAT: f64 = 24_000.0;

/// `(absolute frame, first UMP word)` of every event the sink received.
type Seen = Arc<Mutex<Vec<(u64, u32)>>>;

struct Sink(Seen);

impl Node for Sink {
    fn shape(&self) -> Shape {
        Shape::audio(
            tutti_core::ChannelLayout::EMPTY,
            tutti_core::ChannelLayout::EMPTY,
        )
        .with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
        let mut seen = self.0.lock().unwrap();
        for e in io.events(0) {
            let EventKind::Midi(Ump(w)) = e.kind else {
                panic!("a clip sends MIDI")
            };
            seen.push((cx.env.frame.get() + u64::from(e.offset.get()), w[0]));
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

fn beat_of(frame: u64) -> Beat {
    Beat(frame as f64 / FRAMES_PER_BEAT)
}

fn on(note: u8) -> MidiEvent {
    MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, note, 0xFFFF)
}

fn off(note: u8) -> MidiEvent {
    MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, note, 0)
}

fn at(frame: u64, event: MidiEvent) -> TimedMidiEvent {
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
    Transport::new(true, Bpm(120.0), beat_of(frame), None)
}

/// Render `frames` in blocks of `block`, rolling from frame 0.
fn roll(exec: &mut Executor, frames: u64, block: usize) {
    let mut f = 0;
    while f < frames {
        exec.process(block, &rolling(f), &[], &mut []);
        f += block as u64;
    }
}

/// The first word of a note-on / note-off for `note` on group 0, channel 0.
fn word(e: MidiEvent) -> u32 {
    e.data[0]
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
        vec![
            (24, word(on(60))),
            (200, word(off(60))),
            (300, word(on(62)))
        ]
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
    let looping = |f: u64| Transport::new(true, Bpm(120.0), beat_of(f), lp);
    exec.process(100, &looping(23_800), &[], &mut []);
    exec.process(512, &looping(23_900), &[], &mut []);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            (50, word(on(60))),
            (150, word(on(64))),
            (200, word(off(60))),
            (200, word(off(64))),
            (248, word(on(62))),
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
    let stopped = Transport::new(false, Bpm(120.0), beat_of(64), None);
    exec.process(64, &stopped, &[], &mut []);
    exec.process(64, &stopped, &[], &mut []);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![(10, word(on(60))), (64, word(off(60)))]
    );
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
        vec![(10, word(on(60))), (64, word(off(60))), (100, word(on(72)))]
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
    assert_eq!(*seen.lock().unwrap(), vec![(300, word(on(60)))]);
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
    let looping = |f: u64| Transport::new(true, Bpm(120.0), beat_of(f), lp);
    exec.process(100, &looping(23_800), &[], &mut []);
    exec.process(100, &looping(23_900), &[], &mut []);
    exec.process(100, &looping(0), &[], &mut []);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![(50, word(on(60))), (200, word(off(60)))]
    );
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
    assert_eq!(*seen.lock().unwrap(), vec![(100, word(on(60)))]);
}

/// **Note-offs the port refuses go out in the blocks after, until none is
/// held.** 400 notes are started over two blocks (200 each, under the
/// port's 256); a seek then owes 400 note-offs: 256 on the seek block's
/// first frame, the other 144 on the next block's, which rolls on from the
/// seek (continuous: nothing but the debt would release them).
///
/// Mutation: never retry an owed release → 144 notes hang → fails.
#[test]
fn refused_note_offs_are_retried_until_none_is_held() {
    let note = |i: u16| {
        MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::new((i / 128) as u8),
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
        .filter(|(_, w)| (w >> 20) & 0xf == 0x8)
        .map(|&(f, _)| f)
        .collect();
    assert_eq!(offs.len(), 400, "every started note is ended");
    assert_eq!(offs.iter().filter(|&&f| f == 512).count(), 256);
    assert_eq!(offs.iter().filter(|&&f| f == 768).count(), 144);
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
    assert_eq!(*seen.lock().unwrap(), vec![(10, word(on(60)))]);
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
        let t = Transport::new(true, Bpm(90.0), Beat(f as f64 / 32_000.0), None);
        exec.process(64, &t, &[], &mut []);
        f += 64;
    }
    assert_eq!(
        *seen.lock().unwrap(),
        vec![(32_000, word(on(60))), (96_000, word(on(62)))]
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
    let looping = |f: u64| Transport::new(true, Bpm(120.0), beat_of(f), lp);
    exec.process(512, &looping(0), &[], &mut []);
    exec.process(512, &looping(12), &[], &mut []);
    let mut want: Vec<(u64, u32)> = (0..11)
        .map(|k| (10 + 100 * k, word(on(60))))
        .chain((0..10).map(|k| (60 + 100 * k, word(off(60)))))
        .collect();
    want.sort_unstable();
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
/// `HeldNotes::play`) → the stream is on, on, off: a stacking receiver keeps
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
        vec![
            (10, word(on(60))),
            (20, word(off(60))),
            (20, word(on(60))),
            (64, word(off(60))),
        ]
    );
    // A receiver that starts a voice per note-on and ends one per note-off.
    let voices = seen.iter().fold(0i32, |v, &(_, w)| match (w >> 20) & 0xf {
        0x9 => v + 1,
        0x8 => (v - 1).max(0),
        _ => v,
    });
    assert_eq!(voices, 0, "nothing left sounding");
}

/// **Overlapping notes of one key: the newer keeps its full length.** A is
/// on 0..100, B (the same key) on 50..150. The stream is A on at 0, A ended
/// and B on at 50, and B's note-off at 150: nothing at 100 (A's own
/// note-off, swallowed), so B is not cut there and no stray note-off
/// follows it.
///
/// Mutation (run): never swallow a restarted key's note-off (the
/// `restarted` check dropped) → a note-off at 100 cuts B → fails.
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
        vec![
            (0, word(on(60))),
            (50, word(off(60))),
            (50, word(on(60))),
            (150, word(off(60))),
        ]
    );
}

/// A loop of `[start, end)` beats under `tempo`, rolling from `beat`.
fn looping_at(tempo: f64, beat: Beat, start: Beat, end: Beat) -> Transport {
    Transport::new(true, Bpm(tempo), beat, Some(LoopRange { start, end }))
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
        .filter(|&&(_, w)| w == word(off(60)))
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
    let frames = |w: u32| -> Vec<u64> {
        seen.iter()
            .filter(|&&(_, x)| x == w)
            .map(|&(f, _)| f)
            .collect()
    };
    assert_eq!(
        frames(word(on(60))),
        vec![10, 110, 210, 280, 330, 380, 430, 480]
    );
    assert_eq!(
        frames(word(off(60))),
        vec![60, 160, 255, 305, 355, 405, 455, 505]
    );
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
    use tutti_core::{FrameClock, LoopRange as Region, FRAME_TOLERANCE};
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
    let mut clock = FrameClock::new(Beat(2.999), tutti_core::Bpm(TEMPO), SampleRate(RATE));
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
    let mut walk = FrameClock::new(Beat(2.999), tutti_core::Bpm(TEMPO), SampleRate(RATE));
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
                want.push((f, word(on(60 + i as u8))));
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
    let ons: Vec<u32> = (0..notes.len()).map(|i| word(on(60 + i as u8))).collect();
    let mut got: Vec<_> = seen
        .lock()
        .unwrap()
        .iter()
        .copied()
        .filter(|(_, w)| ons.contains(w))
        .collect();
    got.sort_unstable();
    want.sort_unstable();
    assert_eq!(got, want);
}

/// Renders four 64-frame blocks of a clip with one note at the loop's start,
/// rolling from it, on a thread, and returns what the sink saw, or `None`
/// when that takes longer than a generous deadline.
fn sub_frame_loop(start: Beat, end: Beat) -> Option<Vec<(u64, u32)>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let clip = MidiClipNode::new([TimedMidiEvent::new(start, on(60))]);
        let (_ed, mut exec, _c, seen) = rig(clip, 64);
        let mut clock =
            tutti_core::FrameClock::new(start, tutti_core::Bpm(120.0), SampleRate(RATE));
        let region = tutti_core::LoopRange::new(start, end).unwrap();
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
        let ons = seen.iter().filter(|&&(_, w)| w == word(on(60))).count();
        let offs = seen.iter().filter(|&&(_, w)| w == word(off(60))).count();
        assert_eq!((ons, offs), (256, 255), "[{start:?}, {end:?})");
    }
}
