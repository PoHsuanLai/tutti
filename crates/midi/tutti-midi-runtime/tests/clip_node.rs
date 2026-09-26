//! [`MidiClipNode`]: a clip plays its notes out of an event port, on the
//! frames playback reaches their beats in the block's `Env` — through a seek,
//! a loop wrap, a stop and a replaced clip — and ends the notes it leaves
//! sounding when playback jumps (doc 013, rewrite item 5).
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
