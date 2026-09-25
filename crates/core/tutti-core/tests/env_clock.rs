//! The beat signal on the native graph (doc 013 Phase 3, PR 6).
//!
//! What is pinned here:
//!
//! - `EnvClock` in a graph engine emits the beat of each frame's `Env`
//!   under the whole transport vocabulary: play, timestamped seeks, a loop
//!   wrap (and a loop armed behind the playhead, which does not jump), stop
//!   and start, tempo steps inside a block, untimed edits between blocks,
//!   and ragged device blocks;
//! - `ClickNode` fed by `EnvClock` clicks on the frames the beat model
//!   reaches each beat on;
//! - an offline render driven by `OfflineTimeline::render_graph` hands the
//!   graph the transport a live graph engine hands it for the same timeline:
//!   the `Env` per block, and the timeline a `Legacy` clip reader polls per
//!   64-frame chunk.
//!
//! Until doc 013 PR 15 the first two compared the graph against a `Net`
//! engine with a `TransportClock`, bit for bit. With the engine's `Net`
//! backend gone, the oracles are the graph's own `Env` (a second code path:
//! `Env::transport_at`'s closed form, against `EnvClock`'s walk of the
//! clock's `FrameClock`) and the closed-form beat model in `support`.

use std::sync::{Arc, Mutex};

mod support;

use support::{model_beats, Change};
use tutti_core::{
    At, AudioUnit, Beat, Bpm, BufferMut, BufferRef, ChannelLayout, ClickNode, ClickSettings,
    Engine, EnvClock, FadeOut, Frame, InterleavedMut, LoopRange, MetronomeMode, MotionEvent,
    OfflineTimeline, OfflineTimelineConfig, SampleRate, Samples, Signal, SignalFrame, Tail, Then,
    Timeline, Transport, TransportCommand,
};
use tutti_graph::{Cx, Editor, Env, Executor, IntoNode, Io, Legacy, Node, Prepare, Shape, Status};
use tutti_types::graph::{Edge, InPort, OutPort, Source};
use tutti_types::NodeKey;

const SR: f64 = 48_000.0;

type Log = Arc<Mutex<Vec<(f32, f32)>>>;

/// Ragged device blocks, some past the graph's 512-frame maximum (split into
/// graph blocks) and some not multiples of 64 (`Legacy`'s chunk).
fn blocks(total: usize) -> Vec<usize> {
    let pattern = [512usize, 300, 77, 1024, 511, 64, 1, 900];
    let mut out = Vec::new();
    let mut done = 0;
    for &n in pattern.iter().cycle() {
        if done >= total {
            break;
        }
        out.push(n);
        done += n;
    }
    out
}

// ---- the graph -------------------------------------------------------------

/// Two inputs (the beat ports), one silent output: logs, for every frame,
/// the port pair and the beat the block's `Env` gives the frame
/// (`transport_at`).
struct GraphBeats(Log, Arc<Mutex<Vec<f64>>>);

impl GraphBeats {
    fn new(log: &Log) -> Self {
        Self(Arc::clone(log), Arc::default())
    }
}

impl Node for GraphBeats {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::STEREO, ChannelLayout::MONO).with_tail(Tail::Unbounded)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let mut log = self.0.lock().expect("log");
        let mut env = self.1.lock().expect("log");
        for k in cx.env.offsets() {
            let i = k.index();
            log.push((io.input(0)[i], io.input(1)[i]));
            env.push(cx.env.transport_at(k).beat().get());
        }
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// Wire an `EnvClock` at key 1 into `sink` (key 2)'s two inputs, and `sink`'s
/// `width` outputs to the globals.
fn wire_clock(ed: &mut Editor, sink: impl IntoNode<Controls = ()>, width: u16) {
    const CLOCK: NodeKey = NodeKey(1);
    const SINK: NodeKey = NodeKey(2);
    ed.insert(CLOCK, "clock", EnvClock::new());
    ed.insert(SINK, "sink", sink);
    let topology = &mut ed.spec_mut().topology;
    for port in 0..2 {
        topology.edges.insert(
            InPort { node: SINK, port },
            Edge::Direct(Source::Node(OutPort { node: CLOCK, port })),
        );
    }
    topology.outputs = (0..width)
        .map(|port| Source::Node(OutPort { node: SINK, port }))
        .collect();
    ed.commit().expect("commits");
}

/// A graph engine over `transport`: an `EnvClock` feeding `sink`.
fn graph_engine(
    transport: &Transport,
    sink: impl IntoNode<Controls = ()>,
    width: u16,
) -> (Engine, Editor) {
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(SR), Samples(512)));
    wire_clock(&mut ed, sink, width);
    let engine = Engine::new(transport, &mut ed, exec).expect("within the limits");
    (engine, ed)
}

fn render(engine: &Engine, layout: ChannelLayout, n: usize) -> Vec<f32> {
    let mut buf = vec![0.0f32; n * layout.count() as usize];
    engine.process(&mut InterleavedMut::new(&mut buf, layout));
    buf
}

fn beat(p: (f32, f32)) -> f64 {
    tutti_core::beat_from_ports(p.0, p.1).get()
}

// ---- EnvClock carries the Env's beat ----------------------------------------

/// The whole transport vocabulary, driven through a graph engine with an
/// `EnvClock`: every frame's port pair is the beat that frame's `Env` gives
/// (`transport_at`, a closed form written apart from the clock's walk), to
/// the bit on each block's first frame and within the `f32` split's
/// rounding (1e-7 beat) everywhere else; and the events land where they
/// were meant to. Commands land inside blocks (the graph block is not
/// split), so this is the segment walk, not only the block start.
///
/// Until doc 013 PR 15 the oracle was a `TransportClock` in a `Net` engine,
/// bit for bit per frame. `EnvClock` runs that clock's own code
/// (`FrameClock`) from each piece's origin, so the ports equal it by
/// construction; `transport_at` agrees with it to rounding (it folds a loop
/// by the unwrapped position), and the frame checks below pin each event.
///
/// Mutations (run), each fails:
/// - use the block's `env.transport` for every piece in `EnvClock` (ignore
///   the cuts) → wrong from the first mid-block command (frame 700);
/// - hand `EnvClock`'s walk no loop region → it never wraps → wrong from
///   the first wrap (~1 600);
/// - step the beat while stopped → the stop at 8 000 keeps moving.
#[test]
fn env_clock_emits_the_env_beat() {
    let transport = Transport::new(SR);
    let log = Log::default();
    let sink = GraphBeats::new(&log);
    let env_beats = Arc::clone(&sink.1);
    let (graph, _ed) = graph_engine(&transport, sink, 1);

    let locate = |beat: f64| MotionEvent::Locate {
        beat: Beat(beat),
        fade: FadeOut::Immediate,
        then: Then::Keep,
    };
    let m = &transport.motion;
    m.try_send(MotionEvent::Play).expect("room");
    let at = |f: u64| At::Frame(Frame(f));
    m.schedule(at(700), TransportCommand::Tempo(Bpm(97.0)))
        .expect("room");
    // Armed with the playhead inside it (~0.03): wraps from ~1 600.
    m.schedule(
        at(1_000),
        TransportCommand::Loop(LoopRange::new(0.02, 0.06)),
    )
    .expect("room");
    m.schedule(at(2_900), TransportCommand::Loop(None))
        .expect("room");
    m.schedule(at(3_100), locate(3.25)).expect("room");
    // Armed behind the playhead (~3.28): no jump.
    m.schedule(at(4_000), TransportCommand::Loop(LoopRange::new(1.0, 2.0)))
        .expect("room");
    // Into it, just before its end: wraps ~300 frames on.
    m.schedule(at(6_000), locate(1.99)).expect("room");
    m.schedule(at(8_000), MotionEvent::stop_now())
        .expect("room");
    m.schedule(at(9_500), MotionEvent::Play).expect("room");
    // A beat-timed tempo step, mid-block.
    m.schedule(At::Beat(Beat(1.5)), TransportCommand::Tempo(Bpm(140.0)))
        .expect("room");
    m.schedule(at(30_000), TransportCommand::Loop(None))
        .expect("room");
    let blocks = blocks(40_000);
    let mut starts = Vec::new();
    let mut f = 0;
    for (i, &n) in blocks.iter().enumerate() {
        if i == 20 {
            // An untimed edit between blocks, as a UI makes one.
            transport.settings.set_tempo(Bpm(133.0));
        }
        render(&graph, ChannelLayout::MONO, n);
        starts.push(f);
        f += n;
    }
    let graph_log = log.lock().expect("log");
    let env_beats = env_beats.lock().expect("log");
    let frames: usize = blocks.iter().sum();
    assert_eq!(graph_log.len(), frames);
    assert_eq!(env_beats.len(), frames);
    for (f, (&p, &e)) in graph_log.iter().zip(env_beats.iter()).enumerate() {
        assert!(
            (beat(p) - e).abs() < 1e-7,
            "frame {f}: ports {p:?} ({}), env {e}",
            beat(p)
        );
    }
    // Graph blocks never straddle a device block, so each device block's
    // first frame starts a graph block.
    for &f in &starts {
        let e = env_beats[f];
        let split = (e.floor() as f32, e.fract() as f32);
        assert_eq!(graph_log[f], split, "block starting at frame {f}");
    }

    // Every event happened, where it was meant to.
    let b = |f: usize| beat(graph_log[f]);
    assert!(
        graph_log[1_000..2_900]
            .windows(2)
            .any(|w| beat(w[1]) < beat(w[0])),
        "the first loop wrapped"
    );
    assert_eq!(b(3_100), 3.25, "the seek landed on its frame");
    assert!(b(5_999) > 3.25, "the loop armed behind did not jump");
    assert_eq!(graph_log[6_000], (1.0, 0.99), "the second seek too");
    assert!(b(6_400) < 1.01, "and it wraps once inside it");
    assert_eq!(graph_log[8_000], graph_log[9_499], "stopped from 8 000");
    assert_ne!(graph_log[9_500], graph_log[9_501], "rolling from 9 500");
    // Steps read back through the `f32` split resolve ~1e-7 beat, far finer
    // than the 133 → 140 BPM difference (~2.4e-6 beat a frame).
    let step = |f: usize| b(f + 1) - b(f);
    let fast = (9_500..30_000)
        .find(|&f| step(f) > 136.5 / 60.0 / SR)
        .expect("the tempo step landed");
    assert!(b(fast) >= 1.5 - 1e-6 && b(fast - 1) < 1.5, "on its beat");
    // The first stretch, before any wrap, is the closed form: 120 BPM to
    // 700, then 97.
    let model = model_beats(
        SR,
        120.0,
        &[
            (700, Change::Tempo(97.0)),
            (1_000, Change::Loop(Some((0.02, 0.06)))),
        ],
        2_900,
    );
    for (f, &want) in model.iter().enumerate().take(2_900) {
        assert!((b(f) - want).abs() < 1e-7, "frame {f}: {} vs {want}", b(f));
    }
}

// ---- the metronome ---------------------------------------------------------

/// Frames on which the click starts: silent, then sounding.
fn onsets(stereo: &[f32]) -> Vec<usize> {
    let left: Vec<f32> = stereo.iter().step_by(2).copied().collect();
    (1..left.len())
        .filter(|&f| left[f] != 0.0 && left[f - 1] == 0.0)
        .collect()
}

/// `ClickNode` behind an `EnvClock` (through `Legacy`) clicks on the frame
/// the beat model reaches each new beat on: across a tempo step, a seek and
/// loop wraps, landing inside blocks.
///
/// The transport rolls throughout. `ClickNode` gates on the live play flag,
/// read once per 64-frame chunk; the engine runs every chunk after the whole
/// block's commands are applied, so a mid-block start or stop gates it from
/// the block's first frame, not on its frame. That is `ClickNode`'s own gate
/// (doc 013, gap 5; fixed by its native port), not the beat's.
///
/// Until doc 013 PR 15 the oracle was the same click behind a `TransportClock`
/// in a `Net` engine: the same onset frames and the same samples. The onsets
/// are now the model's (`support::model_beats`): a click starts on the first
/// frame of each beat (frame 0, then each frame whose beat's whole part
/// changes, a seek and a wrap included). `onsets` counts a silent frame
/// followed by a sounding one, and a click's first sample is `sin(0)`, an
/// exact zero on every target, so each is seen one frame after its beat.
/// The samples themselves are `ClickNode`'s (its own tests pin them, and
/// they are `sin` of a phase, which is libm), so they are not pinned here.
///
/// Mutation (run): hold the first segment's transport for the whole block in
/// `EnvClock` (ignore the cuts) → the clicks after the seek at 30 000 land on
/// other frames → fails.
#[test]
fn click_behind_env_clock_clicks_on_the_model_beats() {
    let transport = Transport::new(SR);
    let settings = Arc::new(ClickSettings::new());
    settings.set_mode(MetronomeMode::Always);
    settings.set_volume(1.0);
    let click = ClickNode::with_transport(transport.clone(), settings, SR);
    let (graph, _ed) = graph_engine(&transport, Legacy::new(click), 2);
    let m = &transport.motion;
    m.try_send(MotionEvent::Play).expect("room");
    m.schedule(
        At::Frame(Frame(20_111)),
        TransportCommand::Tempo(Bpm(151.0)),
    )
    .expect("room");
    m.schedule(
        At::Frame(Frame(30_000)),
        MotionEvent::Locate {
            beat: Beat(6.9),
            fade: FadeOut::Immediate,
            then: Then::Keep,
        },
    )
    .expect("room");
    m.schedule(
        At::Frame(Frame(30_001)),
        TransportCommand::Loop(LoopRange::new(6.5, 8.25)),
    )
    .expect("room");
    let mut out = Vec::new();
    for n in blocks(150_000) {
        out.extend(render(&graph, ChannelLayout::STEREO, n));
    }
    let frames = out.len() / 2;
    let model = model_beats(
        SR,
        120.0,
        &[
            (20_111, Change::Tempo(151.0)),
            (30_000, Change::Seek(6.9)),
            (30_001, Change::Loop(Some((6.5, 8.25)))),
        ],
        frames as u64,
    );
    let beats =
        std::iter::once(0).chain((1..frames).filter(|&f| model[f].floor() != model[f - 1].floor()));
    let want: Vec<usize> = beats.map(|f| f + 1).collect();
    let got = onsets(&out);
    assert_eq!(got, want, "onset frames");
    // Not vacuous: clicks before the tempo step, after the seek, and after
    // the loop wraps (7 and 8, then 7 and 8 again).
    assert!(got.len() >= 8, "{got:?}");
    assert!(got.iter().any(|&f| f < 20_000));
    assert!(got.iter().filter(|&&f| f > 30_000).count() >= 4);
}

// ---- offline ---------------------------------------------------------------

/// Logs every block's `Env`.
struct EnvLog(Arc<Mutex<Vec<Env>>>);

impl Node for EnvLog {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO).with_tail(Tail::Unbounded)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        self.0.lock().expect("log").push(*cx.env);
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// A clip reader's view of time: an `AudioUnit` (so a `Legacy` node, called
/// per 64-frame chunk) that logs what its `Timeline` reads on every call,
/// the way a sampler voice polls its own.
#[derive(Clone)]
struct TimelinePoll {
    timeline: Arc<dyn Timeline>,
    log: Arc<Mutex<Vec<f64>>>,
}

impl AudioUnit for TimelinePoll {
    fn inputs(&self) -> usize {
        0
    }
    fn outputs(&self) -> usize {
        1
    }
    fn tick(&mut self, _: &[f32], output: &mut [f32]) {
        output[0] = 0.0;
    }
    fn process(&mut self, size: usize, _: &BufferRef, output: &mut BufferMut) {
        self.log
            .lock()
            .expect("log")
            .push(self.timeline.beat().get());
        for i in 0..size {
            output.set_f32(0, i, 0.0);
        }
    }
    fn route(&mut self, _: &SignalFrame, _: f64) -> SignalFrame {
        let mut out = SignalFrame::new(1);
        out.set(0, Signal::Latency(0.0));
        out
    }
    fn get_id(&self) -> u64 {
        tutti_core::mnemonic(b"TTLNPOLL")
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

/// An `EnvLog` at key 1 and a `Legacy` `TimelinePoll` over `timeline` at
/// key 2, each on a global output.
fn env_graph(
    max_block: usize,
    log: &Arc<Mutex<Vec<Env>>>,
    timeline: Arc<dyn Timeline>,
    polls: &Arc<Mutex<Vec<f64>>>,
) -> (Editor, Executor) {
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(SR), Samples(max_block)));
    ed.insert(NodeKey(1), "env", EnvLog(Arc::clone(log)));
    ed.insert(
        NodeKey(2),
        "clip",
        Legacy::new(TimelinePoll {
            timeline,
            log: Arc::clone(polls),
        }),
    );
    ed.spec_mut().topology.outputs = [1, 2]
        .map(|k| {
            Source::Node(OutPort {
                node: NodeKey(k),
                port: 0,
            })
        })
        .to_vec();
    ed.commit().expect("commits");
    (ed, exec)
}

/// An offline render from bar 2 (beat 4), at 100 BPM, looping beats 6–7,
/// driven by `OfflineTimeline::render_graph`, hands the graph the time a
/// live graph engine hands it for the same timeline:
///
/// - **per block, the `Env`**: the same play state, tempo and loop, no
///   changes, and the same start beat;
/// - **per block, the timeline a clip reader polls** (its
///   `Arc<dyn Timeline>`, read on every `AudioUnit::process`): the `Env`'s
///   beat at the block's first frame, on both sides.
///
/// The graph holds a `Legacy` unit, so both render **chunk-major**: blocks
/// of at most 64 frames (doc 013's `Legacy` compatibility mode), which is
/// what makes the polled timeline right for every call, and every block
/// here is checked to be one. The blocks correspond one to one because the
/// offline side replays the live side's. The beats agree within a
/// tolerance, not to the bit: the live clock accumulates frame by frame and
/// the offline timeline a block at a time, the same split as
/// `OfflineTimeline` vs `TransportClock`.
///
/// Mutations (run):
/// - `advance(frames)` before `graph_block` in `render_graph` → every
///   offline beat is one block ahead → fails on block 0;
/// - drop the loop from `graph_block` → the offline beats run past 7 →
///   fails once the live one wraps;
/// - the engine rendering whole blocks with a `Legacy` unit present
///   (`GraphRender::settle` ignoring `has_legacy`) → blocks past 64
///   frames → fails the chunk-major check;
/// - the engine publishing its playhead in the walk, before the render
///   (`TransportClock::advance` writing back) → every live poll reads its
///   block's end → fails on block 1.
#[test]
fn an_offline_render_sees_the_live_engine_transport() {
    const BAR_2: f64 = 4.0;
    let blocks = blocks(96_000);

    // Live.
    let live_t = Transport::new(SR);
    let live_log = Arc::new(Mutex::new(Vec::new()));
    let live_polls = Arc::new(Mutex::new(Vec::new()));
    let (mut ed, exec) = env_graph(512, &live_log, Arc::new(live_t.clone()), &live_polls);
    let live = Engine::new(&live_t, &mut ed, exec).expect("within the limits");
    live_t.settings.set_tempo(Bpm(100.0));
    live_t.settings.loop_span.set_range(6.0, 7.0);
    live_t.settings.loop_span.set_enabled(true);
    live_t
        .motion
        .try_send(MotionEvent::locate_and_play(Beat(BAR_2)))
        .expect("room");
    for &n in &blocks {
        render(&live, ChannelLayout::MONO, n);
    }

    // Offline, in the graph blocks the live engine rendered (a device block
    // past its maximum is two).
    let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        start_beat: Beat(BAR_2),
        tempo: Bpm(100.0),
        sample_rate: SampleRate(SR),
        loop_range: LoopRange::new(6.0, 7.0),
    }));
    let off_log = Arc::new(Mutex::new(Vec::new()));
    let off_polls = Arc::new(Mutex::new(Vec::new()));
    let (_ed, mut exec) = env_graph(512, &off_log, timeline.clone(), &off_polls);
    let (mut o1, mut o2) = (vec![0.0f32; 512], vec![0.0f32; 512]);
    let live_log = live_log.lock().expect("log");
    for env in live_log.iter() {
        let n = env.block_len.get();
        timeline.render_graph(&mut exec, n, &[], &mut [&mut o1[..n], &mut o2[..n]]);
    }
    let off_log = off_log.lock().expect("log");

    assert_eq!(off_log.len(), live_log.len());
    let mut wrapped = false;
    for (i, (live, off)) in live_log.iter().zip(off_log.iter()).enumerate() {
        assert_eq!(off.frame, live.frame, "block {i}");
        assert_eq!(off.block_len, live.block_len, "block {i}");
        assert!(off.changes.is_empty() && live.changes.is_empty());
        let (l, o) = (live.transport, off.transport);
        assert_eq!(
            (o.playing, o.tempo, o.looping),
            (l.playing, l.tempo, l.looping),
            "block {i}"
        );
        assert!(
            (o.beat().get() - l.beat().get()).abs() < 1e-9,
            "block {i}: offline {}, live {}",
            o.beat().get(),
            l.beat().get()
        );
        wrapped |= i > 0 && l.beat() < live_log[i - 1].transport.beat();
    }

    // Chunk-major: no block past `LEGACY_CHUNK`, on either side.
    for (i, env) in live_log.iter().enumerate() {
        assert!(
            env.block_len.get() <= tutti_graph::LEGACY_CHUNK,
            "block {i} is {} frames: a graph holding a `Legacy` unit renders \
             chunk-major",
            env.block_len.get()
        );
    }

    // What the clip reader polled, both sides, against the `Env` at the
    // block's first frame: one poll a block.
    let (live_polls, off_polls) = (
        live_polls.lock().expect("log"),
        off_polls.lock().expect("log"),
    );
    assert_eq!(live_polls.len(), live_log.len(), "one live poll a block");
    assert_eq!(off_polls.len(), live_log.len(), "one offline poll a block");
    for (i, (env, (&l, &o))) in live_log
        .iter()
        .zip(live_polls.iter().zip(off_polls.iter()))
        .enumerate()
    {
        let want = env.transport.beat().get();
        assert!(
            (o - want).abs() < 1e-9 && (l - want).abs() < 1e-9,
            "block {i} (at frame {}): live polled {l}, offline {o}, the Env says {want}",
            env.frame.get()
        );
    }

    // Not vacuous: it started on bar 2, it rolled, and it wrapped.
    assert_eq!(off_log[0].transport.beat(), Beat(BAR_2));
    assert!(off_log[0].transport.playing);
    assert!(wrapped, "the loop wrapped");
    // The timeline stands after the last block, where the live clock does.
    assert!((timeline.beat().get() - live_t.settings.beat().get()).abs() < 1e-9);
}

/// The beat an `EnvClock` emits in an offline render starts every block on
/// the timeline's own playhead, the figure clip readers and samplers holding
/// the timeline read: the two clocks of an offline render agree at every
/// block start, bit for bit, and within a block to rounding.
///
/// Mutation (run): `advance(frames)` before `graph_block` in
/// `render_graph` → the first emitted beat is one block on → fails on
/// block 0.
#[test]
fn env_clock_offline_starts_each_block_on_the_timeline() {
    let timeline = OfflineTimeline::new(&OfflineTimelineConfig {
        start_beat: Beat(4.0),
        tempo: Bpm(128.0),
        sample_rate: SampleRate(SR),
        loop_range: None,
    });
    let log = Log::default();
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(SR), Samples(512)));
    wire_clock(&mut ed, GraphBeats::new(&log), 1);
    let mut out = vec![0.0f32; 512];
    let mut at = 0;
    for n in [512usize, 300, 77, 512, 1, 200] {
        let start = timeline.beat();
        timeline.render_graph(&mut exec, n, &[], &mut [&mut out[..n]]);
        let log = log.lock().expect("log");
        let split = (start.floor().get() as f32, start.fract().get() as f32);
        assert_eq!(log[at], split, "block starting at frame {at}");
        let last = beat(log[at + n - 1]);
        let expect = start.get() + (n - 1) as f64 * timeline.beats_per_sample().get();
        assert!((last - expect).abs() < 1e-6, "frame {}", at + n - 1);
        at += n;
    }
}
