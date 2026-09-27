//! [`HarmonyNode`]: chord and scale changes go out of an event port on the
//! frames playback reaches their beats, and the context in force is
//! re-stated wherever playback jumps (a start, a seek, a loop wrap) or the
//! lanes are replaced.
//!
//! Every test renders a harmony node into a sink that logs each harmony
//! event with its absolute frame. 120 BPM at 48 kHz: a beat is 24 000 frames.

use std::sync::{Arc, Mutex};

use tutti_core::graph::{OutPort, Source};
use tutti_core::{Beat, Bpm, NodeKey, SampleRate, Samples};
use tutti_graph::{
    Cx, Editor, EventEdge, EventIn, EventKind, EventOut, Executor, ForkMode, ForkTarget, Harmony,
    Io, LoopRange, Node, Prepare, Shape, Status, Transport,
};
use tutti_midi_runtime::{HarmonyControls, HarmonyNode, TimedHarmony};

const RATE: f64 = 48_000.0;
const FRAMES_PER_BEAT: f64 = 24_000.0;

/// `(absolute frame, harmony)` of every harmony event the sink received.
type Seen = Arc<Mutex<Vec<(u64, Harmony)>>>;

/// Logs harmony; a silent mono output so a fork targeting it has one. A
/// fork's clone logs into the same list.
#[derive(Clone)]
struct Sink(Seen);

impl Node for Sink {
    fn shape(&self) -> Shape {
        Shape::audio(
            tutti_core::ChannelLayout::EMPTY,
            tutti_core::ChannelLayout::MONO,
        )
        .with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let mut seen = self.0.lock().unwrap();
        for e in io.events(0) {
            if let EventKind::Harmony(h) = e.kind {
                seen.push((cx.env.frame.get() + u64::from(e.offset.get()), h));
            }
        }
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

fn rig(node: HarmonyNode, max: usize) -> (Editor, Executor, HarmonyControls, Seen) {
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(RATE), Samples(max)));
    let controls = ed.insert(NodeKey(1), "harmony", node);
    let seen = Seen::default();
    ed.insert(
        NodeKey(2),
        "sink",
        tutti_graph::ForkByClone(Sink(Arc::clone(&seen))),
    );
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
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort {
        node: NodeKey(2),
        port: 0,
    })];
    ed.commit().expect("commits");
    (ed, exec, controls, seen)
}

fn beat_of(frame: u64) -> Beat {
    Beat(frame as f64 / FRAMES_PER_BEAT)
}

fn at(frame: u64, h: Harmony) -> TimedHarmony {
    TimedHarmony::new(beat_of(frame), h)
}

fn rolling(frame: u64) -> Transport {
    Transport::new(true, Bpm(120.0), beat_of(frame), None)
}

/// Render one block of `len` frames with the transport at `frame`.
fn block(exec: &mut Executor, len: usize, t: Transport) {
    let mut out = vec![0.0f32; len];
    exec.process(len, &t, &[], &mut [&mut out[..]]);
}

const C: Harmony = Harmony::chord(60, 60, 0b1001_0001);
const G: Harmony = Harmony::chord(67, 67, 0b1001_0001);
const F_OVER_A: Harmony = Harmony::chord(65, 69, 0b1001_0001);
const C_MAJOR: Harmony = Harmony::scale(60, 0b1010_1011_0101);
const A_MINOR: Harmony = Harmony::scale(69, 0b0101_1010_1101);

/// A chord progression and a key change over two beats.
fn lanes() -> Vec<TimedHarmony> {
    vec![
        at(0, C),
        at(0, C_MAJOR),
        at(10_000, G),
        at(24_000, F_OVER_A),
        at(30_000, A_MINOR),
    ]
}

/// **Changes land on their frames, and a continuous run sends each once.**
/// From frame 0 in 512-frame blocks through frame 32 768: the chord and scale
/// at 0, G at 10 000, F/A at 24 000, A minor at 30 000; nothing re-stated.
///
/// Mutation: treat every segment as a jump (the walk never continuous) → the
/// context is re-stated each block → fails. Mutation: place changes at
/// offset 0 of their block → fails.
#[test]
fn changes_land_on_their_frames_and_are_sent_once() {
    let (_ed, mut exec, _c, seen) = rig(HarmonyNode::new(lanes()), 512);
    for b in 0..64 {
        block(&mut exec, 512, rolling(b * 512));
    }
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            (0, C),
            (0, C_MAJOR),
            (10_000, G),
            (24_000, F_OVER_A),
            (30_000, A_MINOR)
        ]
    );
}

/// **A start mid-lane re-states the context in force**, on the start's
/// frame: rolling from frame 20 000 (between G and F/A), the first block
/// sends G and C major at 0, then F/A on its frame.
///
/// Mutation: no re-statement at a jump → only F/A arrives → fails.
#[test]
fn a_start_mid_lane_restates_the_context() {
    let (_ed, mut exec, _c, seen) = rig(HarmonyNode::new(lanes()), 512);
    for b in 0..10 {
        block(&mut exec, 512, rolling(20_000 + b * 512));
    }
    let seen = seen.lock().unwrap();
    assert_eq!(seen[..2], [(0, G), (0, C_MAJOR)]);
    assert_eq!(seen[2], (4_000, F_OVER_A));
}

/// **A seek re-states the context there; a stop sends nothing.** Rolling
/// from 0, stopped for a block, then a seek to frame 31 000: A minor and F/A
/// in force at the seek, sent on its frame (the executor's 1 536).
///
/// Mutation: send something when stopped (`Sending::stop`) → fails.
#[test]
fn a_seek_restates_the_context_and_a_stop_sends_nothing() {
    let (_ed, mut exec, _c, seen) = rig(HarmonyNode::new(lanes()), 512);
    block(&mut exec, 512, rolling(0));
    block(
        &mut exec,
        512,
        Transport::new(false, Bpm(120.0), beat_of(512), None),
    );
    block(&mut exec, 512, rolling(31_000));
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 4, "{seen:?}");
    assert_eq!(seen[2..], [(1_024, F_OVER_A), (1_024, A_MINOR)]);
}

/// **A loop wrap inside a block re-states the context at the wrap.** Loop
/// [0.5, 1.5) beats (frames 12 000–36 000); a block from 35 800 wraps 200
/// frames in: at the wrap (loop start, beat 0.5) G and C major are in force.
///
/// Mutation: no jump at a wrap → nothing at the wrap → fails.
#[test]
fn a_loop_wrap_restates_the_context_at_the_wrap() {
    let (_ed, mut exec, _c, seen) = rig(HarmonyNode::new(lanes()), 512);
    let lp = Some(LoopRange {
        start: Beat(0.5),
        end: Beat(1.5),
    });
    block(
        &mut exec,
        512,
        Transport::new(true, Bpm(120.0), beat_of(35_800), lp),
    );
    let seen = seen.lock().unwrap();
    let at_wrap: Vec<Harmony> = seen.iter().filter(|s| s.0 == 200).map(|s| s.1).collect();
    assert_eq!(at_wrap, vec![G, C_MAJOR], "{seen:?}");
}

/// **New lanes re-state from the next block; the same lanes cut nothing.**
///
/// Mutation: `set` publishing whatever the lanes → the same lanes re-state
/// the context → fails. Mutation: a new generation not forgetting the walk →
/// the new lanes' context is not re-stated → fails.
#[test]
fn replaced_lanes_restate_and_the_same_lanes_do_not() {
    let (_ed, mut exec, controls, seen) = rig(HarmonyNode::new(lanes()), 512);
    block(&mut exec, 512, rolling(0));
    // The same changes, beat order kept (at one beat, order is meaning).
    controls.set(lanes());
    block(&mut exec, 512, rolling(512));
    assert_eq!(seen.lock().unwrap().len(), 2, "the same lanes: nothing new");
    controls.set([at(0, G)]);
    block(&mut exec, 512, rolling(1_024));
    assert_eq!(seen.lock().unwrap()[2..], [(1_024, G)]);
    assert_eq!(controls.len(), 1);
}

/// **A fork sends the lanes as they stood.** A fork taken, then the live
/// lanes replaced: the fork re-states C and C major on its first block.
///
/// Mutation: the fork sharing the live cell → it sends G → fails.
#[test]
fn a_fork_sends_the_lanes_as_they_stood() {
    let (live, _exec, controls, seen) = rig(HarmonyNode::new(lanes()), 512);
    let prepare = *live.prepare();
    let (_fork, mut fork_exec) = live
        .fork(ForkTarget::Master, ForkMode::Live, prepare)
        .expect("forks");
    controls.set([at(0, G)]);
    block(&mut fork_exec, 512, rolling(0));
    assert_eq!(seen.lock().unwrap()[..], [(0, C), (0, C_MAJOR)]);
}
