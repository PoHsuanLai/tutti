//! The beat signal on the native graph (doc 013 Phase 3, PR 6).
//!
//! What is pinned here:
//!
//! - `EnvClock` in a graph engine emits, bit for bit, what a
//!   `TransportClock` in a `Net` engine emits under the same transport:
//!   play, timestamped seeks, a loop wrap (and a loop armed behind the
//!   playhead, which does not jump), stop and start, tempo steps inside a
//!   block, untimed edits between blocks, and ragged device blocks;
//! - `ClickNode` fed by `EnvClock` clicks on the same frames, with the same
//!   samples, as fed by `TransportClock`;
//! - an offline render driven by `OfflineTimeline::render_graph` hands the
//!   graph the transport a live graph engine hands it for the same timeline:
//!   the `Env` per block, and the timeline a `Legacy` clip reader polls per
//!   64-frame chunk.

use std::sync::{Arc, Mutex};

use tutti_core::dsp::Net;
use tutti_core::{
    At, AudioUnit, Beat, Bpm, BufferMut, BufferRef, ChannelLayout, ClickNode, ClickSettings,
    Engine, EnvClock, FadeOut, Frame, InterleavedMut, LoopRange, MetronomeMode, MotionEvent,
    OfflineTimeline, OfflineTimelineConfig, SampleRate, Samples, Signal, SignalFrame, Tail, Then,
    Timeline, Transport, TransportClock, TransportCommand,
};
use tutti_graph::{Cx, Editor, Env, Executor, IntoNode, Io, Legacy, Node, Prepare, Shape, Status};
use tutti_types::graph::{Edge, InPort, OutPort, Source};
use tutti_types::NodeKey;

const SR: f64 = 48_000.0;

type Log = Arc<Mutex<Vec<(f32, f32)>>>;

/// Ragged device blocks, some past the graph's 512-frame maximum (split into
/// graph blocks) and some not multiples of 64 (the `Net`'s chunk).
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

// ---- the two backends ------------------------------------------------------

/// Two inputs (the beat ports), one silent output: logs the beat of every
/// frame. The `Net` side.
#[derive(Clone)]
struct NetBeats(Log);

impl AudioUnit for NetBeats {
    fn inputs(&self) -> usize {
        2
    }
    fn outputs(&self) -> usize {
        1
    }
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        self.0.lock().expect("log").push((input[0], input[1]));
        output[0] = 0.0;
    }
    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        let mut log = self.0.lock().expect("log");
        for i in 0..size {
            log.push((input.channel_f32(0)[i], input.channel_f32(1)[i]));
            output.set_f32(0, i, 0.0);
        }
    }
    fn route(&mut self, _: &SignalFrame, _: f64) -> SignalFrame {
        let mut out = SignalFrame::new(1);
        out.set(0, Signal::Latency(0.0));
        out
    }
    fn tail(&mut self) -> Tail {
        Tail::Unbounded
    }
    fn get_id(&self) -> u64 {
        tutti_core::mnemonic(b"TNETBEAT")
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

/// The same, as a native graph node.
struct GraphBeats(Log);

impl Node for GraphBeats {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::STEREO, ChannelLayout::MONO).with_tail(Tail::Unbounded)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let mut log = self.0.lock().expect("log");
        for i in 0..io.frames() {
            log.push((io.input(0)[i], io.input(1)[i]));
        }
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// A `Net` engine over `transport`: a `TransportClock` feeding `sink`'s two
/// inputs, `sink`'s outputs the root's.
fn net_engine(transport: &Transport, sink: Box<dyn AudioUnit>) -> Engine {
    let width = sink.outputs();
    let mut net = Net::new(0, width);
    let clock = net.push(Box::new(TransportClock::new(transport.clock_links(), SR)));
    let sink = net.push(sink);
    net.connect(clock, 0, sink, 0);
    net.connect(clock, 1, sink, 1);
    for c in 0..width {
        net.connect_output(sink, c, c);
    }
    net.set_sample_rate(SampleRate(SR));
    let backend = net.backend();
    // The backend is fed through the net; keep the frontend alive.
    Box::leak(Box::new(net));
    Engine::new(transport.motion.clone(), backend)
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
    let engine = Engine::with_graph(transport, &mut ed, exec).expect("within the limits");
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

// ---- EnvClock == TransportClock --------------------------------------------

/// The whole transport vocabulary, driven through a `Net` engine with a
/// `TransportClock` and a graph engine with an `EnvClock`: every frame's
/// port pair is bit-equal. Commands land inside blocks (the graph block is
/// not split; the `Net` is rendered piece by piece), so this is the
/// segment walk, not only the block start.
///
/// Mutations (run), each fails on the first frame after the event:
/// - use the block's `env.transport` for the whole block instead of walking
///   `segments` → wrong from the first mid-block command (frame 700);
/// - wrap with `LoopRange::wrap` instead of `advance` → the loop armed
///   behind the playhead at frame 4 000 jumps into [1, 2);
/// - step the beat while stopped → the stop at 8 000 keeps moving.
///
/// Not caught, and not claimed: reading `env.transport_at(k)` instead of
/// continuing the clock's segment. It is the same closed form up to a loop
/// wrap and agrees to rounding past one, which the `f32` split at the ports
/// rounds away; `EnvClock` runs the clock's own code (`FrameClock`) so the
/// ports equal the clock's by construction rather than by that rounding.
#[test]
fn env_clock_emits_what_transport_clock_emits() {
    let net_t = Transport::new(SR);
    let net_log = Log::default();
    let net = net_engine(&net_t, Box::new(NetBeats(Arc::clone(&net_log))));
    let graph_t = Transport::new(SR);
    let graph_log = Log::default();
    let (graph, _ed) = graph_engine(&graph_t, GraphBeats(Arc::clone(&graph_log)), 1);

    let locate = |beat: f64| MotionEvent::Locate {
        beat: Beat(beat),
        fade: FadeOut::Immediate,
        then: Then::Keep,
    };
    for t in [&net_t, &graph_t] {
        let m = &t.motion;
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
    }
    let blocks = blocks(40_000);
    for (i, &n) in blocks.iter().enumerate() {
        if i == 20 {
            // An untimed edit between blocks, as a UI makes one.
            net_t.settings.set_tempo(Bpm(133.0));
            graph_t.settings.set_tempo(Bpm(133.0));
        }
        render(&net, ChannelLayout::MONO, n);
        render(&graph, ChannelLayout::MONO, n);
    }
    let net_log = net_log.lock().expect("log");
    let graph_log = graph_log.lock().expect("log");
    let frames: usize = blocks.iter().sum();
    assert_eq!(net_log.len(), frames);
    assert_eq!(graph_log.len(), frames);
    for (f, (n, g)) in net_log.iter().zip(graph_log.iter()).enumerate() {
        assert_eq!(
            (n.0.to_bits(), n.1.to_bits()),
            (g.0.to_bits(), g.1.to_bits()),
            "frame {f}: net {n:?}, graph {g:?}"
        );
    }

    // Not vacuous: every event happened, where it was meant to.
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
}

// ---- the metronome ---------------------------------------------------------

/// Frames on which the click starts: silent, then sounding.
fn onsets(stereo: &[f32]) -> Vec<usize> {
    let left: Vec<f32> = stereo.iter().step_by(2).copied().collect();
    (1..left.len())
        .filter(|&f| left[f] != 0.0 && left[f - 1] == 0.0)
        .collect()
}

/// `ClickNode` behind an `EnvClock` (through `Legacy`) clicks on the same
/// frames, with the same samples, as behind a `TransportClock`: across a
/// tempo step, a seek and a loop wrap, landing inside blocks.
///
/// The transport rolls throughout. `ClickNode` gates on the live play flag,
/// read once per 64-frame chunk; on the graph backend every chunk runs after
/// the whole block's commands are applied, so a mid-block start or stop
/// gates it from the block's first frame, not on its frame. That is
/// `ClickNode`'s own gate (doc 013, gap 5; fixed by its native port), not
/// the beat's.
///
/// Mutation (run): hold the first segment's transport for the whole block in
/// `EnvClock` (ignore `segments`) → the clicks after the seek at 30 000 land
/// on other frames → fails.
#[test]
fn click_behind_env_clock_clicks_on_the_same_frames() {
    let click = |t: &Transport| {
        let settings = Arc::new(ClickSettings::new());
        settings.set_mode(MetronomeMode::Always);
        settings.set_volume(1.0);
        ClickNode::with_transport(t.clone(), settings, SR)
    };
    let net_t = Transport::new(SR);
    let net = net_engine(&net_t, Box::new(click(&net_t)));
    let graph_t = Transport::new(SR);
    let (graph, _ed) = graph_engine(&graph_t, Legacy::new(click(&graph_t)), 2);
    for t in [&net_t, &graph_t] {
        let m = &t.motion;
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
    }
    let mut net_out = Vec::new();
    let mut graph_out = Vec::new();
    for n in blocks(150_000) {
        net_out.extend(render(&net, ChannelLayout::STEREO, n));
        graph_out.extend(render(&graph, ChannelLayout::STEREO, n));
    }
    let net_on = onsets(&net_out);
    let graph_on = onsets(&graph_out);
    assert_eq!(net_on, graph_on, "onset frames");
    // Not vacuous: clicks before the tempo step, after the seek, and after
    // the loop wraps (7 and 8, then 7 and 8 again).
    assert!(net_on.len() >= 8, "{net_on:?}");
    assert!(net_on.iter().any(|&f| f < 20_000));
    assert!(net_on.iter().filter(|&&f| f > 30_000).count() >= 4);
    assert!(
        net_out
            .iter()
            .zip(&graph_out)
            .all(|(a, b)| a.to_bits() == b.to_bits()),
        "the same samples"
    );
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
    let live = Engine::with_graph(&live_t, &mut ed, exec).expect("within the limits");
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
            (o.beat.get() - l.beat.get()).abs() < 1e-9,
            "block {i}: offline {}, live {}",
            o.beat.get(),
            l.beat.get()
        );
        wrapped |= i > 0 && l.beat < live_log[i - 1].transport.beat;
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
        let want = env.transport.beat.get();
        assert!(
            (o - want).abs() < 1e-9 && (l - want).abs() < 1e-9,
            "block {i} (at frame {}): live polled {l}, offline {o}, the Env says {want}",
            env.frame.get()
        );
    }

    // Not vacuous: it started on bar 2, it rolled, and it wrapped.
    assert_eq!(off_log[0].transport.beat, Beat(BAR_2));
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
    wire_clock(&mut ed, GraphBeats(Arc::clone(&log)), 1);
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
