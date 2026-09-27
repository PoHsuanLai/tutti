//! **A placed clip enters on its frame** (the frame is the source of truth)
//! — not a chunk late, and not a block late.
//!
//! At 90 BPM and 48 kHz a frame is 1/32 000 of a beat, which binary cannot
//! represent. A clock that accumulated its beat (`beat += beats_per_sample`)
//! would drift off frame 96 000's beat, which is exactly 3 (to
//! `2.999999999999891`), and a placement gate comparing beats
//! (`beat < start` → silent) would hold a clip placed at beat 3 silent past
//! its frame. Three properties, each pinned here:
//!
//! - the clocks count frames and derive the beat in closed form, so a node's
//!   `Env` reads beat 3 **to the bit** on frame 96 000, on every path;
//! - the gate asks whether playback has reached the start **by frame**
//!   (`TimelineSegment::reached_by`), so a start an ulp after the playhead's
//!   beat on its frame (two roundings of one musical position) still enters
//!   there;
//! - the voice reads the transport from its block's `Env`, **per frame**, so
//!   it enters on its frame wherever that falls in the block, not at the next
//!   64-frame boundary.
//!
//! A clip at beat 3, a constant wave: its first non-zero frame is exactly
//! 96 000 on every path a voice is driven by — live, the engine
//! (`Engine::new`) in 441-frame device blocks; offline, a graph rendered by
//! `OfflineTimeline::render_graph` in 1023-frame blocks; and a node driven
//! by hand in 77-frame blocks. On none is 96 000 the first frame of a block,
//! or of a 64-frame piece of one (a voice renders its blocks 64 frames at a
//! time from their start): it falls 47, 29 and 58 frames into a piece.
//!
//! Mutations (run):
//! - accumulate in `FrameClock::advance` (restart the segment on every call
//!   at `beat + frames × beats_per_sample`) → no path's `Env` reads beat 3 on
//!   frame 96 000 → the three path tests fail on the probe. (The entry frame
//!   itself survives that mutation: the gate's tolerance absorbs an error of
//!   3.5e-9 frames. The drift grows with the session; the exact read is the
//!   property.)
//! - compare beats in the gate (`Gate::reached` as `now >= target`) → the
//!   start an ulp late enters a frame late → `a_start_an_ulp_after_the_frame_
//!   enters_on_it` fails.
//! - `place` gating only a range's first frame (a per-call gate) → every
//!   path enters at the next 64-frame piece → all fail. (The block lengths
//!   are chosen so 96 000 is never a piece's first frame; where it is, this
//!   mutation passes.)

use std::sync::{Arc, Mutex};

use tutti_core::graph::{OutPort, Source};
use tutti_core::{
    Beat, Bpm, ChannelLayout, Engine, Frame, FrameClock, InterleavedMut, MotionEvent, NodeKey,
    OfflineTimeline, OfflineTimelineConfig, SampleRate, Samples, Transport,
};
use tutti_graph::{contract, Cx, Editor, Env, Io, Node, Prepare, Shape, Status, TransportChanges};
use tutti_io::Wave;
use tutti_sampler::{MemorySource, SlotId, Voice, VoicePool, VoiceSource};

const SR: f64 = 48_000.0;
const TEMPO: Bpm = Bpm(90.0);
/// Where the clip starts, and the frame that is exactly that beat at 90 BPM:
/// 3 × 60 × 48 000 / 90.
const START: Beat = Beat(3.0);
const START_FRAME: usize = 96_000;
/// Rendered: past the start by a few device blocks.
const FRAMES: usize = 98_304;
/// The block lengths of the three paths: none puts 96 000 on a block's, or
/// a 64-frame piece's, first frame.
const DEVICE: usize = 441;
const OFFLINE: usize = 1023;
const HAND: usize = 77;

/// A pool with one voice placed at `start`, playing a constant (so its first
/// frame is already non-zero).
fn pool(start: Beat) -> VoicePool {
    let mut wave = Wave::new(1, SR);
    for _ in 0..SR as usize {
        wave.push_frame(&[0.5]);
    }
    let source = MemorySource::placed(Arc::new(wave), start, None);
    let mut pool = VoicePool::new();
    pool.insert_voice(
        SlotId(1),
        Voice {
            source: VoiceSource::Memory(source),
            play: Default::default(),
            channel_index: None,
        },
    );
    pool
}

/// A node that records the beat its block's `Env` gives the frame the clip
/// starts on: what the voice beside it reads there. No outputs.
struct Probe(Arc<Mutex<Option<Beat>>>);

impl Node for Probe {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        if let Some(o) = cx.env.offset_of(Frame(START_FRAME as u64)) {
            *self.0.lock().expect("probe") = Some(cx.env.transport_at(o).beat());
        }
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// What a path rendered: the left channel, and the beat frame 96 000 read.
struct Run {
    left: Vec<f32>,
    read: Option<Beat>,
}

fn transport() -> Transport {
    let t = Transport::new(SR);
    t.settings.set_tempo(TEMPO);
    t
}

/// Render `FRAMES` of `engine` in [`DEVICE`]-frame device blocks, playing; the
/// left channel.
fn play(engine: &Engine, t: &Transport) -> Vec<f32> {
    t.motion.try_send(MotionEvent::Play).expect("room");
    let mut left = Vec::with_capacity(FRAMES);
    let mut buf = vec![0.0f32; DEVICE * 2];
    while left.len() < FRAMES {
        engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
        left.extend(buf.iter().step_by(2));
    }
    left
}

/// The pool on the outputs, and a probe beside it.
fn graph_with(pool: VoicePool, probe: Arc<Mutex<Option<Beat>>>) -> (Editor, tutti_graph::Executor) {
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(SR), Samples(1024)));
    ed.insert(NodeKey(1), "voice", pool);
    ed.insert(NodeKey(2), "probe", tutti_graph::Unforkable(Probe(probe)));
    ed.spec_mut().topology.outputs = (0..2)
        .map(|port| {
            Source::Node(OutPort {
                node: NodeKey(1),
                port,
            })
        })
        .collect();
    ed.commit().expect("commits");
    (ed, exec)
}

fn live_graph(start: Beat) -> Run {
    let t = transport();
    let probe = Arc::default();
    let (mut ed, exec) = graph_with(pool(start), Arc::clone(&probe));
    let engine = Engine::new(&t, &mut ed, exec).expect("within the limits");
    let left = play(&engine, &t);
    let read = *probe.lock().expect("probe");
    Run { left, read }
}

fn offline_graph(start: Beat) -> Run {
    let timeline = OfflineTimeline::new(&OfflineTimelineConfig {
        start_beat: Beat(0.0),
        tempo: TEMPO,
        sample_rate: SampleRate(SR),
        loop_range: None,
    });
    let probe = Arc::default();
    let (_ed, mut exec) = graph_with(pool(start), Arc::clone(&probe));
    let mut left = Vec::with_capacity(FRAMES);
    let (mut l, mut r) = (vec![0.0f32; OFFLINE], vec![0.0f32; OFFLINE]);
    while left.len() < FRAMES {
        timeline.render_graph(&mut exec, OFFLINE, &[], &mut [&mut l[..], &mut r[..]]);
        left.extend_from_slice(&l);
    }
    let read = *probe.lock().expect("probe");
    Run { left, read }
}

/// The pool driven by hand in [`HAND`]-frame blocks, its `Env` from a host frame
/// clock (counted, as an engine's).
fn by_hand(start: Beat) -> Run {
    let mut node = contract::prepared(pool(start), SampleRate(SR), HAND);
    let mut probe = Probe(Arc::default());
    let mut host = FrameClock::new(Beat(0.0), TEMPO, SampleRate(SR));
    let mut left = Vec::with_capacity(FRAMES);
    while left.len() < FRAMES {
        let env = Env {
            frame: Frame(left.len() as u64),
            sample_rate: SampleRate(SR),
            block_len: Samples(HAND),
            transport: tutti_graph::Transport::counted(true, TEMPO, host.origin(), None),
            changes: TransportChanges::NONE,
        };
        left.extend_from_slice(&contract::drive_in(&mut node, &env, &[], &[], &[]).audio[0]);
        contract::drive_in(&mut probe, &env, &[], &[], &[]);
        host.advance(Samples(HAND), None);
    }
    let read = *probe.0.lock().expect("probe");
    Run { left, read }
}

/// The clip enters on `frame`.
fn assert_enters_at(what: &str, run: &Run, frame: usize) {
    assert_eq!(
        run.left.iter().position(|&s| s != 0.0),
        Some(frame),
        "{what}: the clip must enter on frame {frame}"
    );
}

/// Frame 96 000's `Env` reads exactly beat 3, not a rounding of it.
fn assert_read_start_exactly(what: &str, run: &Run) {
    let read = run.read.expect("the render reached frame 96 000");
    assert_eq!(
        read.get().to_bits(),
        START.get().to_bits(),
        "{what}: frame {START_FRAME} read {read:?}, not {START:?}"
    );
}

#[test]
fn a_clip_enters_on_its_frame_live_through_the_graph() {
    let run = live_graph(START);
    assert_enters_at("live, graph", &run, START_FRAME);
    assert_read_start_exactly("live, graph", &run);
}

#[test]
fn a_clip_enters_on_its_frame_offline_through_the_graph() {
    let run = offline_graph(START);
    assert_enters_at("offline, graph", &run, START_FRAME);
    assert_read_start_exactly("offline, graph", &run);
}

#[test]
fn a_clip_enters_on_its_frame_driven_by_hand() {
    let run = by_hand(START);
    assert_enters_at("by hand", &run, START_FRAME);
    assert_read_start_exactly("by hand", &run);
}

/// The gate's own rule, apart from the clock: a clip whose start is an ulp
/// after the beat the clock reads on its frame still enters on that frame.
/// Two computations of one musical position (a clip start from a project
/// file, a playhead from the frame count) can differ by an ulp; a beat
/// comparison turned that into a frame. A frame's worth later is the next
/// frame — the rule's tolerance is a millionth of a frame — **not the next
/// chunk**: until the voice read its `Env` per frame, that clip entered at
/// 96 064.
#[test]
fn a_start_an_ulp_after_the_frame_enters_on_it() {
    let start = Beat(f64::from_bits(START.get().to_bits() + 1));
    assert_enters_at("an ulp late", &offline_graph(start), START_FRAME);
    let later = Beat(START.get() + 1.0 / 32_000.0);
    assert_enters_at("a frame late", &offline_graph(later), START_FRAME + 1);
    assert_enters_at("a frame late, by hand", &by_hand(later), START_FRAME + 1);
}
