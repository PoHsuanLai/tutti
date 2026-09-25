//! **A placed clip enters on its frame, not a chunk late** (doc 013 §6, "the
//! frame is the source of truth").
//!
//! At 90 BPM and 48 kHz a frame is 1/32 000 of a beat, which binary cannot
//! represent. A clock that accumulated its beat (`beat += beats_per_sample`,
//! per frame live, per 64-frame chunk offline) drifted off frame 96 000's
//! beat, which is exactly 3: offline it read `2.999999999999891` there, and
//! the placement gate (`beat < start` → silent) held a clip placed at beat 3
//! silent for that whole 64-frame call. Two fixes, each pinned here:
//!
//! - the clocks count frames and derive the beat in closed form, so the voice
//!   reads beat 3 **to the bit** on frame 96 000's chunk, on every path;
//! - the gate asks whether playback has reached the start **by frame**
//!   (`TimelineSegment::reached_by`), so a start an ulp after the playhead's
//!   beat on its frame (two roundings of one musical position) still enters
//!   there.
//!
//! A clip at beat 3, a constant wave: its first non-zero frame is exactly
//! 96 000 on every path a voice is driven by:
//!
//! - live, the `Net` engine (a `TransportClock` node) and the native graph
//!   engine (`Engine::with_graph`, the voice a `Legacy` node, chunk-major);
//! - offline, a `Net`-style render (the voice called per 64 frames, then the
//!   `OfflineTimeline` advanced, as the export's `NetSource` does) and
//!   `OfflineTimeline::render_graph` through the native graph.
//!
//! Mutations (run):
//! - accumulate in `FrameClock::advance` (restart the segment on every call
//!   at `beat + frames × beats_per_sample`, so per frame live through the
//!   `Net`, per chunk elsewhere) → every path reads `2.999999999999891`-ish
//!   on frame 96 000's chunk, never beat 3 → all four path tests fail. (The
//!   entry frame itself survives that mutation: the gate's tolerance absorbs
//!   an error of 3.5e-9 frames. The drift grows with the session; the exact
//!   read is the property.)
//! - compare beats in `window_position` (`now < start_beat` → `None`) → the
//!   start an ulp late enters at 96 064 → `a_start_an_ulp_after_the_frame_
//!   enters_on_it` fails.

use std::sync::{Arc, Mutex};

use tutti_core::dsp::Net;
use tutti_core::graph::{OutPort, Source};
use tutti_core::{
    AudioUnit, Beat, Bpm, BufferVec, ChannelLayout, Engine, InterleavedMut, MotionEvent, NodeKey,
    OfflineTimeline, OfflineTimelineConfig, SampleRate, Samples, Timeline, Transport,
    TransportClock,
};
use tutti_graph::{Editor, Legacy, Prepare};
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

/// A timeline that records every beat read from it: what the voice saw.
struct Logged {
    inner: Arc<dyn Timeline>,
    reads: Mutex<Vec<Beat>>,
}

impl Timeline for Logged {
    fn beat(&self) -> Beat {
        let beat = self.inner.beat();
        self.reads.lock().expect("reads").push(beat);
        beat
    }
    fn tempo(&self) -> Bpm {
        self.inner.tempo()
    }
    fn is_rolling(&self) -> bool {
        self.inner.is_rolling()
    }
}

fn logged(inner: Arc<dyn Timeline>) -> Arc<Logged> {
    Arc::new(Logged {
        inner,
        reads: Mutex::new(Vec::new()),
    })
}

/// A pool on `timeline` with one voice placed at `start`, playing a constant
/// (so its first frame is already non-zero).
fn pool(timeline: Arc<dyn Timeline>, start: Beat) -> VoicePool {
    let mut wave = Wave::new(1, SR);
    for _ in 0..SR as usize {
        wave.push_frame(&[0.5]);
    }
    let source = MemorySource::with_transport(Arc::new(wave), Arc::clone(&timeline), start, None);
    let (mut pool, _handle) = VoicePool::with_transport(timeline, None);
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

/// What a path rendered: the left channel, and every beat the voice read.
struct Run {
    left: Vec<f32>,
    reads: Vec<Beat>,
}

impl Run {
    fn of(left: Vec<f32>, log: &Logged) -> Self {
        Self {
            left,
            reads: log.reads.lock().expect("reads").clone(),
        }
    }
}

fn transport() -> Transport {
    let t = Transport::new(SR);
    t.settings.set_tempo(TEMPO);
    t
}

/// Render `FRAMES` of `engine` in 512-frame device blocks, playing; the
/// left channel.
fn play(engine: &Engine, t: &Transport) -> Vec<f32> {
    t.motion.try_send(MotionEvent::Play).expect("room");
    let mut left = Vec::with_capacity(FRAMES);
    let mut buf = vec![0.0f32; 512 * 2];
    while left.len() < FRAMES {
        engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
        left.extend(buf.iter().step_by(2));
    }
    left
}

fn live_net(start: Beat) -> Run {
    let t = transport();
    let log = logged(Arc::new(t.clone()));
    let mut net = Net::new(0, 2);
    // The clock first, as `graph_engine_clock.rs` and bevy-tutti's build
    // push it: the voice reads the beat of its chunk's first frame.
    net.push(Box::new(TransportClock::new(t.clock_links(), SR)));
    let voice = net.push(Box::new(pool(Arc::clone(&log) as Arc<dyn Timeline>, start)));
    net.connect_output(voice, 0, 0);
    net.connect_output(voice, 1, 1);
    net.set_sample_rate(SampleRate(SR));
    let backend = net.backend();
    // The backend is fed through the net; keep the frontend alive.
    Box::leak(Box::new(net));
    let engine = Engine::new(t.motion.clone(), backend);
    Run::of(play(&engine, &t), &log)
}

fn graph_with(pool: VoicePool) -> (Editor, tutti_graph::Executor) {
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(SR), Samples(1024)));
    ed.insert(NodeKey(1), "voice", Legacy::new(pool));
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
    let log = logged(Arc::new(t.clone()));
    let (mut ed, exec) = graph_with(pool(Arc::clone(&log) as Arc<dyn Timeline>, start));
    let engine = Engine::with_graph(&t, &mut ed, exec).expect("within the limits");
    Run::of(play(&engine, &t), &log)
}

fn offline_timeline() -> Arc<OfflineTimeline> {
    Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        start_beat: Beat(0.0),
        tempo: TEMPO,
        sample_rate: SampleRate(SR),
        loop_range: None,
    }))
}

fn offline_net(start: Beat) -> Run {
    let timeline = offline_timeline();
    let log = logged(Arc::clone(&timeline) as Arc<dyn Timeline>);
    let mut voice = pool(Arc::clone(&log) as Arc<dyn Timeline>, start);
    voice.set_sample_rate(SampleRate(SR));
    let input = BufferVec::new(0);
    let mut output = BufferVec::new(2);
    let mut left = Vec::with_capacity(FRAMES);
    while left.len() < FRAMES {
        // A `Net` renders 64 frames a call; the export advances after each.
        voice.process(64, &input.buffer_ref(), &mut output.buffer_mut());
        left.extend_from_slice(&output.buffer_ref().channel_f32(0)[..64]);
        timeline.advance(64);
    }
    Run::of(left, &log)
}

fn offline_graph(start: Beat) -> Run {
    let timeline = offline_timeline();
    let log = logged(Arc::clone(&timeline) as Arc<dyn Timeline>);
    let (_ed, mut exec) = graph_with(pool(Arc::clone(&log) as Arc<dyn Timeline>, start));
    let mut left = Vec::with_capacity(FRAMES);
    let (mut l, mut r) = (vec![0.0f32; 1024], vec![0.0f32; 1024]);
    while left.len() < FRAMES {
        timeline.render_graph(&mut exec, 1024, &[], &mut [&mut l[..], &mut r[..]]);
        left.extend_from_slice(&l);
    }
    Run::of(left, &log)
}

/// The clip enters on `frame`, and — for a start on the playhead's own beat
/// — the voice read that beat to the bit on its chunk.
fn assert_enters_at(what: &str, run: &Run, frame: usize) {
    assert_eq!(
        run.left.iter().position(|&s| s != 0.0),
        Some(frame),
        "{what}: the clip must enter on frame {frame}"
    );
}

/// The first beat the voice read at or past a hair before `START`: the
/// playhead of frame 96 000's chunk. Exactly beat 3, not a rounding of it.
fn assert_read_start_exactly(what: &str, run: &Run) {
    let near = run
        .reads
        .iter()
        .find(|b| b.get() > START.get() - 1e-6)
        .expect("the playhead reached the start");
    assert_eq!(
        near.get().to_bits(),
        START.get().to_bits(),
        "{what}: frame {START_FRAME}'s chunk read {near:?}, not {START:?}"
    );
}

#[test]
fn a_clip_enters_on_its_frame_live_through_the_net() {
    let run = live_net(START);
    assert_enters_at("live, Net", &run, START_FRAME);
    assert_read_start_exactly("live, Net", &run);
}

#[test]
fn a_clip_enters_on_its_frame_live_through_the_graph() {
    let run = live_graph(START);
    assert_enters_at("live, graph", &run, START_FRAME);
    assert_read_start_exactly("live, graph", &run);
}

#[test]
fn a_clip_enters_on_its_frame_offline_through_the_net() {
    let run = offline_net(START);
    assert_enters_at("offline, Net", &run, START_FRAME);
    assert_read_start_exactly("offline, Net", &run);
}

#[test]
fn a_clip_enters_on_its_frame_offline_through_the_graph() {
    let run = offline_graph(START);
    assert_enters_at("offline, graph", &run, START_FRAME);
    assert_read_start_exactly("offline, graph", &run);
}

/// The gate's own rule, apart from the clock: a clip whose start is an ulp
/// after the beat the clock reads on its frame still enters on that frame.
/// Two computations of one musical position (a clip start from a project
/// file, a playhead from the frame count) can differ by an ulp; a beat
/// comparison turned that into a whole chunk. A frame's worth later is the
/// next chunk: the rule's tolerance is a millionth of a frame, not a chunk.
///
/// Mutation: `now < start_beat → None` in `window_position` → the ulp-late
/// start enters at 96 064 → fails.
#[test]
fn a_start_an_ulp_after_the_frame_enters_on_it() {
    let start = Beat(f64::from_bits(START.get().to_bits() + 1));
    assert_enters_at("an ulp late", &offline_net(start), START_FRAME);
    let later = Beat(START.get() + 1.0 / 32_000.0);
    assert_enters_at("a frame late", &offline_net(later), START_FRAME + 64);
}
