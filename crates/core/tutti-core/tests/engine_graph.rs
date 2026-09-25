//! `Engine` over the native graph (doc 013 Phase 2), and timestamped
//! transport commands through both backends.
//!
//! What is pinned here:
//!
//! - a graph rendered through `Engine` is the graph rendered by its executor
//!   directly, bit for bit;
//! - the Graph backend folds and declicks exactly as the Net path does;
//! - a transport command at `At::Frame` / `At::Beat` lands on its frame, in a
//!   graph node's `Env` and in a `Net`'s clock alike;
//! - a graph `At::Beat` command scheduled against a timestamped start lands
//!   on its frame — the first engine-level case of the doc 013 §6 contract;
//! - the beat a graph node reads from `Env` is the beat a `Net`'s
//!   `TransportClock` emits.
//!
//! The allocation gate for the Graph backend is in `rt_no_alloc_engine.rs`.

use std::sync::{Arc, Mutex};

use tutti_core::dsp::Net;
use tutti_core::{
    At, AudioUnit, Beat, Bpm, BufferMut, BufferRef, ChannelLayout, Engine, FadeOut, Frame,
    InterleavedMut, LoopRange, MotionEvent, SampleRate, Samples, Signal, SignalFrame, Tail, Then,
    Transport, TransportClock, TransportCommand,
};
use tutti_graph::{
    Cx, Editor, EventIn, EventKind, Executor, Io, Node, Prepare, Shape, Status, Ump,
};
use tutti_types::graph::{OutPort, Source};
use tutti_types::NodeKey;

const SR: f64 = 48_000.0;
/// Frames per beat at 120 BPM and `SR`.
const FPB: u64 = 24_000;

fn prepare(max_block: usize) -> Prepare {
    Prepare::new(SampleRate(SR), Samples(max_block))
}

/// Wire global outputs `0..width` to `node`'s outputs.
fn outputs(ed: &mut Editor, node: NodeKey, width: u16) {
    ed.spec_mut().topology.outputs = (0..width)
        .map(|port| Source::Node(OutPort { node, port }))
        .collect();
}

/// Render `blocks` (frame counts) through `engine` into one interleaved
/// buffer `layout` wide.
fn render(engine: &Engine, layout: ChannelLayout, blocks: &[usize]) -> Vec<f32> {
    let ch = layout.count() as usize;
    let mut all = Vec::new();
    for &n in blocks {
        let mut buf = vec![0.0f32; n * ch];
        engine.process(&mut InterleavedMut::new(&mut buf, layout));
        all.extend_from_slice(&buf);
    }
    all
}

fn bits(v: &[f32]) -> Vec<u32> {
    v.iter().map(|x| x.to_bits()).collect()
}

// ---- graph nodes ---------------------------------------------------------

/// Three outputs that expose what the node was handed: the absolute frame,
/// the block length, and the transport at each frame.
struct Clocked;

impl Node for Clocked {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::from_count(3)).with_tail(Tail::Unbounded)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let env = *cx.env;
        for k in env.offsets() {
            // Exact below 2^24, far past any test here.
            io.output(0)[k.index()] = (env.frame.get() + k.get() as u64) as f32;
            io.output(1)[k.index()] = env.block_len.get() as f32;
            let t = env.transport_at(k);
            io.output(2)[k.index()] =
                if t.playing { 1.0 } else { 0.0 } + t.beat.get().fract() as f32 * 0.5;
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// Per frame: 1 while the transport rolls, else 0. Logs every frame's
/// `(absolute frame, beat, playing)` when given a log.
struct Gate {
    log: Option<Arc<Mutex<Vec<(u64, f64, bool)>>>>,
}

impl Node for Gate {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO).with_tail(Tail::Unbounded)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let env = *cx.env;
        let mut log = self.log.as_ref().map(|l| l.lock().expect("log"));
        for k in env.offsets() {
            let t = env.transport_at(k);
            io.output(0)[k.index()] = if t.playing { 1.0 } else { 0.0 };
            if let Some(log) = log.as_mut() {
                log.push((env.frame_at(k).get(), t.beat.get(), t.playing));
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// One event input; logs the absolute frame of every event it receives.
struct NoteLog(Arc<Mutex<Vec<u64>>>);

impl Node for NoteLog {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
            .with_events(1, 0)
            .with_tail(Tail::Unbounded)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let mut log = self.0.lock().expect("log");
        for e in io.events(0).iter() {
            log.push(cx.env.frame_at(e.offset).get());
        }
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

// ---- legacy units (Net, and the graph through `Legacy`) -------------------

/// `n` outputs of a deterministic, libm-free signal, distinct per channel —
/// so a fold that mixes the wrong channels or drops one shows up.
#[derive(Clone)]
struct Surround {
    channels: usize,
    frame: u64,
}

impl AudioUnit for Surround {
    fn inputs(&self) -> usize {
        0
    }
    fn outputs(&self) -> usize {
        self.channels
    }
    fn reset(&mut self) {
        self.frame = 0;
    }
    fn tick(&mut self, _: &[f32], output: &mut [f32]) {
        for (c, o) in output.iter_mut().enumerate() {
            *o = value(self.frame, c);
        }
        self.frame += 1;
    }
    fn process(&mut self, size: usize, _: &BufferRef, output: &mut BufferMut) {
        for i in 0..size {
            for c in 0..self.channels {
                output.set_f32(c, i, value(self.frame, c));
            }
            self.frame += 1;
        }
    }
    fn route(&mut self, _: &SignalFrame, _: f64) -> SignalFrame {
        let mut out = SignalFrame::new(self.channels);
        for c in 0..self.channels {
            out.set(c, Signal::Latency(0.0));
        }
        out
    }
    fn tail(&mut self) -> Tail {
        Tail::Unbounded
    }
    fn get_id(&self) -> u64 {
        tutti_core::mnemonic(b"TSURRND0")
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

fn value(frame: u64, c: usize) -> f32 {
    ((frame % 97) as f32 - 48.0) / 64.0 * (1.0 + c as f32 * 0.125)
}

/// Two inputs (a clock's beat ports), no output that matters: logs the beat
/// of every frame.
#[derive(Clone)]
struct BeatLog(Arc<Mutex<Vec<(f32, f32)>>>);

impl AudioUnit for BeatLog {
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
        Tail::None
    }
    fn get_id(&self) -> u64 {
        tutti_core::mnemonic(b"TBEATLOG")
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

/// A `Net` engine over `transport` whose root is `root`, with a
/// `TransportClock` feeding `beats` when given.
fn net_engine(
    transport: &Transport,
    root: Box<dyn AudioUnit>,
    beats: Option<Arc<Mutex<Vec<(f32, f32)>>>>,
) -> Engine {
    let width = root.outputs();
    let mut net = Net::new(0, width);
    let id = net.push(root);
    for c in 0..width {
        net.connect_output(id, c, c);
    }
    if let Some(log) = beats {
        let clock = net.push(Box::new(TransportClock::new(transport.clock_links(), SR)));
        let sink = net.push(Box::new(BeatLog(log)));
        net.connect(clock, 0, sink, 0);
        net.connect(clock, 1, sink, 1);
    }
    net.set_sample_rate(SampleRate(SR));
    let backend = net.backend();
    // The backend is fed through the net; keep the frontend alive.
    Box::leak(Box::new(net));
    Engine::new(transport.motion.clone(), backend)
}

/// A graph engine over `transport` with one node at key 1, its outputs wired
/// to the globals. The editor is returned, for scheduling.
fn graph_engine(
    transport: &Transport,
    max_block: usize,
    node: impl Node,
    width: u16,
) -> (Engine, Editor) {
    let (mut ed, exec) = Editor::new(prepare(max_block));
    ed.insert(NodeKey(1), "node", node);
    outputs(&mut ed, NodeKey(1), width);
    ed.commit().expect("commits");
    let engine = Engine::with_graph(transport, &mut ed, exec).expect("within the limits");
    (engine, ed)
}

// ---- the render ----------------------------------------------------------

/// A graph rendered through `Engine` is the graph rendered by its executor,
/// bit for bit: whole device blocks, one executor block each (a device block
/// past the prepared maximum as consecutive maximum-sized ones), the
/// executor's own frame clock, the transport as it stands, and a fold that is
/// the identity at equal width.
///
/// Mutation (run): cap the Graph backend's block at 64 frames
/// (`block_bound`) → channel 1 reads 64 → fails.
#[test]
fn graph_engine_render_is_bit_identical_to_the_executor() {
    let blocks = [64usize, 256, 100, 1024, 1, 512];
    let transport = Transport::new(SR);
    let (engine, _ed) = graph_engine(&transport, 512, Clocked, 3);
    let got = render(&engine, ChannelLayout::from_count(3), &blocks);

    let (mut ed, mut exec): (Editor, Executor) = Editor::new(prepare(512));
    ed.insert(NodeKey(1), "node", Clocked);
    outputs(&mut ed, NodeKey(1), 3);
    ed.commit().expect("commits");
    let mut want = Vec::new();
    for &n in &blocks {
        let mut done = 0;
        while done < n {
            let len = (n - done).min(512);
            let mut planar = vec![vec![0.0f32; len]; 3];
            let mut outs: Vec<&mut [f32]> = planar.iter_mut().map(Vec::as_mut_slice).collect();
            exec.process(len, &tutti_graph::Transport::default(), &[], &mut outs);
            want.extend((0..len).flat_map(|i| planar.iter().map(move |ch| ch[i])));
            done += len;
        }
    }
    assert_eq!(bits(&got), bits(&want));
    // The 1024-frame device block really was two graph blocks.
    let ch1_at = |frame: usize| got[frame * 3 + 1];
    assert_eq!(ch1_at(420 + 512), 512.0);
}

/// The Graph backend folds a root to the device width and declicks exactly
/// as the Net path does: the same source (`Surround`, through `Legacy` in the
/// graph), stereo and 5.1, to stereo and 5.1 devices, rolling and then
/// stopped with a declick fade mid-run — bit-identical output.
///
/// Mutation (run): copy channel `c` straight across instead of `fold_frame`
/// in the graph render → the 6→2 case fails. Skip `walk.ramps.apply` for the
/// graph → the fade differs → fails.
#[test]
fn fold_and_declick_match_the_net_path() {
    let blocks = [256usize, 256, 300, 64, 128, 512, 256];
    for (src, device) in [(2usize, 2u16), (6, 2), (6, 6), (2, 6)] {
        let layout = ChannelLayout::from_count(device);
        let run = |engine: &Engine, transport: &Transport| {
            transport.motion.try_send(MotionEvent::Play).expect("room");
            let mut out = render(engine, layout, &blocks[..3]);
            transport
                .motion
                .try_send(MotionEvent::stop())
                .expect("room");
            out.extend(render(engine, layout, &blocks[3..]));
            out
        };
        let net_t = Transport::new(SR);
        let net = net_engine(
            &net_t,
            Box::new(Surround {
                channels: src,
                frame: 0,
            }),
            None,
        );
        let graph_t = Transport::new(SR);
        let (graph, _ed) = graph_engine(
            &graph_t,
            512,
            tutti_graph::Legacy::new(Surround {
                channels: src,
                frame: 0,
            }),
            src as u16,
        );
        let a = run(&net, &net_t);
        let b = run(&graph, &graph_t);
        assert_eq!(bits(&a), bits(&b), "{src} → {device}");
        // Not vacuous: sound, then the untimed stop (no lead time) lands on
        // the next block's first frame with the gain at zero there, and the
        // ungated source fades back in from it.
        let ch = device as usize;
        let stop = 256 + 256 + 300;
        assert!(a[..stop * ch].iter().any(|&x| x != 0.0));
        assert!(a[stop * ch..(stop + 1) * ch].iter().all(|&x| x == 0.0));
        assert!(a[(stop + 480) * ch..].iter().any(|&x| x != 0.0));
        assert!(net_t.motion.is_stopped() && graph_t.motion.is_stopped());
    }
}

// ---- timestamped transport commands ----------------------------------------

/// `At::Frame` play: the first frame the transport rolls — the first
/// non-silent sample of a transport-gated graph — is exactly that frame, in
/// the middle of a block.
///
/// Mutation (run): land every due command at its piece's first frame
/// (`at = cursor` in the walk) → fails. Hand the executor
/// `TransportChanges::NONE` → the graph hears the start a block late →
/// fails. Drop `schedule.release` → the credit stays held → fails.
#[test]
fn a_timed_start_sounds_from_its_exact_frame() {
    let transport = Transport::new(SR);
    let (engine, _ed) = graph_engine(&transport, 256, Gate { log: None }, 1);
    transport
        .motion
        .schedule(At::Frame(Frame(1_000)), MotionEvent::Play)
        .expect("room");
    let out = render(&engine, ChannelLayout::MONO, &[256; 8]);
    let first = out.iter().position(|&x| x != 0.0);
    assert_eq!(first, Some(1_000));
    assert!(out[1_000..].iter().all(|&x| x == 1.0), "and rolls on");
    assert!(transport.motion.is_playing());
    assert_eq!(transport.motion.scheduled_outstanding(), 0, "credit back");
}

/// The same command through a `Net`: its `TransportClock` holds beat 0
/// through frame 1 000 (emit-then-advance) and has moved one frame's worth
/// of beat at 1 001.
///
/// Mutation (run): land every due command at its piece's first frame
/// (`at = cursor` in the walk) → the clock starts at its block's first
/// frame → fails.
#[test]
fn a_timed_start_moves_a_net_clock_from_its_exact_frame() {
    let transport = Transport::new(SR);
    let beats = Arc::new(Mutex::new(Vec::new()));
    let engine = net_engine(
        &transport,
        Box::new(Surround {
            channels: 1,
            frame: 0,
        }),
        Some(Arc::clone(&beats)),
    );
    transport
        .motion
        .schedule(At::Frame(Frame(1_000)), MotionEvent::Play)
        .expect("room");
    render(&engine, ChannelLayout::MONO, &[256; 8]);
    let beats = beats.lock().expect("log");
    let moving = beats.iter().position(|&(w, f)| w != 0.0 || f != 0.0);
    assert_eq!(moving, Some(1_001));
}

/// Beat-timed seek and stop land on the frame their beat resolves to:
/// rolling from 0 at 120 BPM, a seek at beat 1 is frame 24 000, and a stop at
/// beat 10.5 (half a beat after the seek to 10) is frame 36 000.
///
/// The seek's beat falls on a block's first frame, where the engine's
/// accumulated playhead sits ~1e-12 beat past it: it must land there without
/// counting as late.
///
/// Mutation (run): land every due command at its piece's first frame
/// (`at = cursor` in the walk) → the stop lands at its block's start →
/// fails. Hand the executor `TransportChanges::NONE` → fails. Drop
/// `schedule.release` → `scheduled_outstanding` stays 2 → fails. Drop the
/// behind-side rounding tolerance in `Env::beat_due` → the seek counts late
/// → fails (observed before that tolerance existed).
#[test]
fn beat_timed_seek_and_stop_land_on_their_frames() {
    let transport = Transport::new(SR);
    let log = Arc::new(Mutex::new(Vec::new()));
    let (engine, _ed) = graph_engine(
        &transport,
        700,
        Gate {
            log: Some(Arc::clone(&log)),
        },
        1,
    );
    let m = &transport.motion;
    m.try_send(MotionEvent::Play).expect("room");
    m.schedule(
        At::Beat(Beat(1.0)),
        MotionEvent::Locate {
            beat: Beat(10.0),
            fade: FadeOut::Immediate,
            then: Then::Keep,
        },
    )
    .expect("room");
    m.schedule(At::Beat(Beat(10.5)), MotionEvent::stop_now())
        .expect("room");
    // 500-frame blocks put the seek on a block's first frame; 700-frame ones
    // put the stop inside a block.
    render(&engine, ChannelLayout::MONO, &[500; 48]);
    render(&engine, ChannelLayout::MONO, &[700; 30]);

    let log = log.lock().expect("log");
    let at = |f: u64| log[f as usize];
    assert_eq!(at(FPB).0, FPB);
    assert!((at(FPB - 1).1 - (1.0 - 1.0 / FPB as f64)).abs() < 1e-9);
    assert_eq!(at(FPB).1, 10.0, "the seek lands on beat 1's frame");
    assert!(at(36_000 - 1).2, "rolling up to the stop");
    assert!(!at(36_000).2, "stopped from beat 10.5's frame");
    assert!((at(36_000).1 - 10.5).abs() < 1e-9);
    assert!(m.is_stopped());
    assert_eq!(m.scheduled_outstanding(), 0);
    assert_eq!(m.late_commands(), 0);
}

/// A constant 1.0 that logs the transport at every frame: the declick gain
/// envelope, read straight off the output, beside where the transport moved.
struct DcLog(Arc<Mutex<Vec<(u64, f64, bool)>>>);

impl Node for DcLog {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO).with_tail(Tail::Unbounded)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let env = *cx.env;
        let mut log = self.0.lock().expect("log");
        for k in env.offsets() {
            let t = env.transport_at(k);
            log.push((env.frame_at(k).get(), t.beat.get(), t.playing));
            io.output(0)[k.index()] = 1.0;
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// A constant 1.0 as a `Net` unit.
#[derive(Clone)]
struct Dc;

impl AudioUnit for Dc {
    fn inputs(&self) -> usize {
        0
    }
    fn outputs(&self) -> usize {
        1
    }
    fn tick(&mut self, _: &[f32], output: &mut [f32]) {
        output[0] = 1.0;
    }
    fn process(&mut self, size: usize, _: &BufferRef, output: &mut BufferMut) {
        for i in 0..size {
            output.set_f32(0, i, 1.0);
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
        tutti_core::mnemonic(b"TDCONE00")
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

/// The largest frame-to-frame gain change in `out` (a DC source's output),
/// skipping the step into each frame in `except`.
fn max_step(out: &[f32], except: &[usize]) -> f32 {
    out.windows(2)
        .enumerate()
        .filter(|(i, _)| !except.contains(&(i + 1)))
        .map(|(_, w)| (w[1] - w[0]).abs())
        .fold(0.0, f32::max)
}

/// One fade's step, with room for `f32` rounding.
const FADE_STEP: f32 = 1.0 / 480.0 + 1e-5;

/// Run `script` against a DC source through both backends, rolling from
/// beat 0, in 256-frame blocks for `blocks` blocks; `script(motion, i)` is
/// called before block `i`. Returns, per backend, the output and the
/// transport per frame as `(beat, playing)`: the graph's from its `Env`, the
/// Net's from its clock's ports and published pausedness.
#[allow(clippy::type_complexity)]
fn dc_run(
    blocks: usize,
    script: impl Fn(&tutti_core::MotionFsm, usize),
) -> [(Vec<f32>, Vec<(f64, bool)>); 2] {
    let net_t = Transport::new(SR);
    let beats = Arc::new(Mutex::new(Vec::new()));
    let net = net_engine(&net_t, Box::new(Dc), Some(Arc::clone(&beats)));
    let graph_t = Transport::new(SR);
    let log = Arc::new(Mutex::new(Vec::new()));
    let (graph, _ed) = graph_engine(&graph_t, 256, DcLog(Arc::clone(&log)), 1);
    let mut outs = [Vec::new(), Vec::new()];
    for t in [&net_t, &graph_t] {
        t.motion.try_send(MotionEvent::Play).expect("room");
    }
    for i in 0..blocks {
        script(&net_t.motion, i);
        script(&graph_t.motion, i);
        outs[0].extend(render(&net, ChannelLayout::MONO, &[256]));
        outs[1].extend(render(&graph, ChannelLayout::MONO, &[256]));
    }
    // The Net's clock emits a frame's beat before advancing: playing at
    // frame x means the beat moves from x to x + 1.
    let beats = beats.lock().expect("log");
    let net_t: Vec<(f64, bool)> = beats
        .iter()
        .enumerate()
        .map(|(x, &(w, f))| {
            let moving = beats.get(x + 1).is_some_and(|&n| n != (w, f));
            (w as f64 + f as f64, moving)
        })
        .collect();
    let graph_t: Vec<(f64, bool)> = log
        .lock()
        .expect("log")
        .iter()
        .map(|&(_, b, p)| (b, p))
        .collect();
    let [n, g] = outs;
    [(n, net_t), (g, graph_t)]
}

/// A seek to beat 10 at `At::Frame(1000)`, mid-block, with a declick: the
/// old position's audio fades out over the 480 frames **before** the seek
/// (from 520), reaching zero exactly on frame 1 000, where the transport
/// jumps; the new position's audio fades in from there. The gain never moves
/// by more than one fade step a frame, through both backends.
///
/// Mutation (run): give no lead (`Gain::Aim(None)` at every walk step) →
/// the gain steps from 1 to 0 at 1 000 → fails. Snap the gain back to 1
/// after the jump (no fade-in) → fails.
#[test]
fn a_timed_declicked_seek_fades_out_before_and_in_after_its_frame() {
    let runs = dc_run(12, |m, i| {
        if i == 0 {
            m.schedule(
                At::Frame(Frame(1_000)),
                MotionEvent::Locate {
                    beat: Beat(10.0),
                    fade: FadeOut::Declick,
                    then: Then::Keep,
                },
            )
            .expect("room");
        }
    });
    for (backend, (out, t)) in ["net", "graph"].iter().zip(&runs) {
        assert_eq!(out[519], 1.0, "{backend}: full level before the lead");
        assert!(out[520] < 1.0, "{backend}: the fade-out starts 480 ahead");
        assert_eq!(out[1_000], 0.0, "{backend}: zero on the command frame");
        assert!(out[1_001] > 0.0, "{backend}: the fade-in starts there");
        assert_eq!(out[1_480], 1.0, "{backend}: back to full level");
        assert!(max_step(out, &[]) <= FADE_STEP, "{backend}: continuous");
        // The transport jumps on the command frame.
        assert!(t[999].0 < 1.0, "{backend}: old position up to 999");
        assert_eq!(t[1_000].0, 10.0, "{backend}: beat 10 from frame 1 000");
        assert!(t[1_000].1, "{backend}: still rolling");
    }
}

/// A declicked seek scheduled less than a fade ahead: noticed at frame 1 024
/// (the block after it was sent) for frame 1 224, it fades out over the 200
/// frames that remain (a steeper, still continuous ramp) and lands on its
/// frame.
///
/// Mutation (run): fade out at `1/FADE` a frame regardless of the lead left
/// → the gain is not zero on frame 1 224 → fails. Give no lead → a 1-to-0
/// step → fails.
#[test]
fn a_declicked_seek_with_short_notice_fades_over_what_is_left() {
    let runs = dc_run(12, |m, i| {
        if i == 4 {
            m.schedule(
                At::Frame(Frame(1_224)),
                MotionEvent::Locate {
                    beat: Beat(10.0),
                    fade: FadeOut::Declick,
                    then: Then::Keep,
                },
            )
            .expect("room");
        }
    });
    for (backend, (out, t)) in ["net", "graph"].iter().zip(&runs) {
        assert_eq!(out[1_023], 1.0, "{backend}");
        assert!(
            out[1_024] < 1.0,
            "{backend}: fading from the first frame it is seen"
        );
        assert_eq!(out[1_224], 0.0, "{backend}: zero on the command frame");
        assert!(
            max_step(out, &[]) <= 1.0 / 200.0 + 1e-5,
            "{backend}: continuous"
        );
        assert_eq!(t[1_224].0, 10.0, "{backend}: jumps on its frame");
    }
}

/// A declicked stop at `At::Frame(1000)`: the audio fades out before it, is
/// at zero on frame 1 000 where the transport stops, and the gain recovers
/// from there (the DC source keeps sounding while stopped, as live input or
/// a reverb tail would), continuously.
///
/// Mutation (run): snap the gain back to 1 after the zero (no recovery
/// ramp) → a step → fails. Give no lead → a 1-to-0 step at the stop →
/// fails.
#[test]
fn a_timed_declicked_stop_fades_out_before_its_frame() {
    let runs = dc_run(12, |m, i| {
        if i == 0 {
            m.schedule(At::Frame(Frame(1_000)), MotionEvent::stop())
                .expect("room");
        }
    });
    for (backend, (out, t)) in ["net", "graph"].iter().zip(&runs) {
        assert!(out[520] < 1.0 && out[519] == 1.0, "{backend}");
        assert_eq!(out[1_000], 0.0, "{backend}");
        assert!(max_step(out, &[]) <= FADE_STEP, "{backend}: continuous");
        assert!(t[999].1, "{backend}: rolling up to the stop");
        assert!(!t[1_000].1, "{backend}: stopped on its frame");
    }
}

/// An untimed declicked seek (`At::NextBlock`) has no lead time: it lands on
/// the next block's first frame with the gain at zero there (the old audio
/// ends on that frame, the one step, which the design accepts), and the new
/// audio fades in continuously from it.
///
/// Mutation (run): skip the fade-in after a no-lead jump (gain straight back
/// to 1) → a second step → fails.
#[test]
fn a_next_block_declicked_seek_fades_in_after_the_jump() {
    let runs = dc_run(8, |m, i| {
        if i == 3 {
            m.try_send(MotionEvent::Locate {
                beat: Beat(10.0),
                fade: FadeOut::Declick,
                then: Then::Keep,
            })
            .expect("room");
        }
    });
    for (backend, (out, t)) in ["net", "graph"].iter().zip(&runs) {
        let jump = 3 * 256;
        assert_eq!(out[jump - 1], 1.0, "{backend}: no lead, full level");
        assert_eq!(out[jump], 0.0, "{backend}: zero on the jump frame");
        assert!(
            max_step(out, &[jump]) <= FADE_STEP,
            "{backend}: fade-in continuous"
        );
        assert_eq!(out[jump + 480], 1.0, "{backend}");
        assert_eq!(
            t[jump].0, 10.0,
            "{backend}: jumps on the block's first frame"
        );
    }
}

/// A command whose frame is already past lands at the next block's first
/// frame and is counted late, never dropped.
///
/// Mutation (run): drop `schedule.count_late()` → fails.
#[test]
fn a_past_frame_lands_at_once_and_is_counted() {
    let transport = Transport::new(SR);
    let (engine, _ed) = graph_engine(&transport, 256, Gate { log: None }, 1);
    render(&engine, ChannelLayout::MONO, &[256; 2]);
    transport
        .motion
        .schedule(At::Frame(Frame(10)), MotionEvent::Play)
        .expect("room");
    let out = render(&engine, ChannelLayout::MONO, &[256]);
    assert_eq!(out[0], 1.0, "at the block's first frame");
    assert_eq!(transport.motion.late_commands(), 1);
}

/// Tempo and loop changes at a frame: the beat advances at the old tempo up
/// to it and the new one after, and a loop armed at a frame wraps from then.
///
/// Mutation (run): land every due command at its piece's first frame → the
/// tempo changes at 1 024, not 1 200 → fails.
#[test]
fn tempo_and_loop_change_on_their_frames() {
    let transport = Transport::new(SR);
    let log = Arc::new(Mutex::new(Vec::new()));
    let (engine, _ed) = graph_engine(
        &transport,
        512,
        Gate {
            log: Some(Arc::clone(&log)),
        },
        1,
    );
    let m = &transport.motion;
    m.try_send(MotionEvent::Play).expect("room");
    m.schedule(At::Frame(Frame(1_200)), TransportCommand::Tempo(Bpm(240.0)))
        .expect("room");
    m.schedule(
        At::Frame(Frame(2_000)),
        TransportCommand::Loop(LoopRange::new(0.0, 0.2)),
    )
    .expect("room");
    render(&engine, ChannelLayout::MONO, &[512; 8]);
    let log = log.lock().expect("log");
    let beat = |f: usize| log[f].1;
    // 1 200 frames at 120 BPM = 0.05 beat; then 1/12 000 beat a frame.
    assert!((beat(1_200) - 0.05).abs() < 1e-9);
    assert!((beat(1_800) - 0.1).abs() < 1e-9);
    // The loop 0..0.2 armed at 2 000 (beat 0.1167) wraps where the beat
    // reaches 0.2: 1 000 frames on.
    // Within a frame of rounding either way, as the beat accumulates.
    assert!(beat(2_998) > 0.199);
    assert!(beat(3_002) < 0.001);
    assert!(transport.settings.loop_span.range().is_some());
}

/// The first engine-level case of the doc 013 §6 contract: the transport
/// starts at a frame inside a block (`At::Frame`), and graph notes scheduled
/// at beats land on the frames playback reaches them — beat 0 on the start
/// frame itself, beat 0.5 half a beat later.
///
/// Mutation (run): hand the executor `TransportChanges::NONE` → beat 0 is
/// not reached in the start's block, and lands late at the next block's
/// first frame → fails. Land every due command at its piece's first frame →
/// fails.
#[test]
fn a_graph_beat_note_after_a_timed_start_lands_on_its_frame() {
    let transport = Transport::new(SR);
    let notes = Arc::new(Mutex::new(Vec::new()));
    let (engine, mut ed) = graph_engine(&transport, 256, NoteLog(Arc::clone(&notes)), 1);
    // Install the plan so the editor can schedule against it.
    render(&engine, ChannelLayout::MONO, &[256]);
    ed.collect();
    let to = EventIn {
        node: NodeKey(1),
        port: 0,
    };
    let note = EventKind::Midi(Ump([0x2090_3c64, 0, 0, 0]));
    ed.schedule(At::Beat(Beat(0.0)), to, note).expect("room");
    ed.schedule(At::Beat(Beat(0.5)), to, note).expect("room");
    transport
        .motion
        .schedule(At::Frame(Frame(1_000)), MotionEvent::Play)
        .expect("room");
    render(&engine, ChannelLayout::MONO, &[256; 60]);
    assert_eq!(*notes.lock().expect("log"), vec![1_000, 1_000 + FPB / 2]);
}

/// The beat a graph node reads from `Env` is the beat a `Net`'s
/// `TransportClock` emits, through a start, a tempo change, a loop, a seek
/// and timed commands: at every graph block's first frame and every cut,
/// bit-equal (as the clock's two `f32` ports); everywhere else within 1e-7
/// beat (`transport_at` is closed form and the clock accumulates, and the
/// ports carry the fraction as an `f32`, good to ~6e-8). The published
/// playheads agree to the bit.
///
/// Mutation (run): skip the loop wrap in `TransportClock::advance` → fails
/// after the loop arms. Hand the executor `TransportChanges::NONE` → the
/// per-frame beats differ after a cut → fails. Land every due command at its
/// piece's first frame → fails.
#[test]
fn net_and_graph_see_the_same_beat() {
    let net_t = Transport::new(SR);
    let net_beats = Arc::new(Mutex::new(Vec::new()));
    let net = net_engine(
        &net_t,
        Box::new(Surround {
            channels: 1,
            frame: 0,
        }),
        Some(Arc::clone(&net_beats)),
    );
    let graph_t = Transport::new(SR);
    let graph_log = Arc::new(Mutex::new(Vec::new()));
    let (graph, _ed) = graph_engine(
        &graph_t,
        512,
        Gate {
            log: Some(Arc::clone(&graph_log)),
        },
        1,
    );
    let blocks = [512usize, 300, 512, 77, 512, 512, 400, 512, 512];
    for t in [&net_t, &graph_t] {
        t.motion.try_send(MotionEvent::Play).expect("room");
        t.motion
            .schedule(At::Frame(Frame(1_000)), TransportCommand::Tempo(Bpm(97.0)))
            .expect("room");
        // Armed with the playhead inside it (beat ~0.065): one wrap at
        // ~2 140, disarmed before the next.
        t.motion
            .schedule(
                At::Frame(Frame(1_700)),
                TransportCommand::Loop(LoopRange::new(0.02, 0.08)),
            )
            .expect("room");
        t.motion
            .schedule(At::Frame(Frame(2_900)), TransportCommand::Loop(None))
            .expect("room");
        t.motion
            .schedule(
                At::Frame(Frame(3_100)),
                MotionEvent::Locate {
                    beat: Beat(3.25),
                    fade: FadeOut::Immediate,
                    then: Then::Keep,
                },
            )
            .expect("room");
    }
    for (i, &n) in blocks.iter().enumerate() {
        if i == 6 {
            // An untimed edit between blocks, as a UI makes one.
            net_t.settings.set_tempo(Bpm(133.0));
            graph_t.settings.set_tempo(Bpm(133.0));
        }
        render(&net, ChannelLayout::MONO, &[n]);
        render(&graph, ChannelLayout::MONO, &[n]);
        assert_eq!(
            net_t.settings.beat().get().to_bits(),
            graph_t.settings.beat().get().to_bits(),
            "published playheads, after block {i}"
        );
    }
    let net_beats = net_beats.lock().expect("log");
    let graph_log = graph_log.lock().expect("log");
    assert_eq!(net_beats.len(), graph_log.len());
    let split = |b: f64| (b.floor() as f32, b.fract() as f32);
    let mut exact = vec![0usize, 1_000, 1_700, 2_900, 3_100];
    let mut f = 0;
    for &n in &blocks {
        exact.push(f);
        f += n;
    }
    for &f in &exact {
        assert_eq!(split(graph_log[f].1), net_beats[f], "frame {f}");
    }
    for (f, (&(w, fr), &(_, beat, _))) in net_beats.iter().zip(graph_log.iter()).enumerate() {
        let net = w as f64 + fr as f64;
        assert!(
            (net - beat).abs() < 1e-7,
            "frame {f}: net {net}, graph {beat}"
        );
    }
    // Not vacuous: the seek and the loop happened.
    assert!(graph_log.iter().any(|&(_, b, _)| b >= 3.25));
    let looped = &graph_log[1_700..2_900];
    assert!(looped.iter().all(|&(_, b, _)| b < 0.08));
    assert!(looped.windows(2).any(|w| w[1].1 < w[0].1), "it wrapped");
}

/// A loop armed while the playhead is past its end does not jump: playback
/// runs on, in a `Net`'s clock and in a graph's `Env` alike, until a seek
/// puts the playhead inside the loop, and from then it wraps (doc 013's
/// decision, the common DAW behaviour).
///
/// Mutation (run): make `LoopRange::advance` wrap whenever `to` is past the
/// end (the old `wrap`) → the clock jumps into the loop on the frame after
/// it is armed, the published playhead is inside [1, 2) → fails, and the
/// graph's `Env` beats (linear, by `transport_at`) disagree with the Net's.
#[test]
fn a_loop_armed_behind_the_playhead_does_not_jump() {
    let net_t = Transport::new(SR);
    let net_beats = Arc::new(Mutex::new(Vec::new()));
    let net = net_engine(
        &net_t,
        Box::new(Surround {
            channels: 1,
            frame: 0,
        }),
        Some(Arc::clone(&net_beats)),
    );
    let graph_t = Transport::new(SR);
    let graph_log = Arc::new(Mutex::new(Vec::new()));
    let (graph, _ed) = graph_engine(
        &graph_t,
        512,
        Gate {
            log: Some(Arc::clone(&graph_log)),
        },
        1,
    );
    for t in [&net_t, &graph_t] {
        let m = &t.motion;
        m.try_send(MotionEvent::locate_and_play(Beat(3.0)))
            .expect("room");
        // Armed at beat ~3.04, behind the playhead.
        m.schedule(
            At::Frame(Frame(1_000)),
            TransportCommand::Loop(LoopRange::new(1.0, 2.0)),
        )
        .expect("room");
        // Into the loop, just before its end: a wrap 2 400 frames later.
        m.schedule(
            At::Frame(Frame(20_000)),
            MotionEvent::Locate {
                beat: Beat(1.9),
                fade: FadeOut::Immediate,
                then: Then::Keep,
            },
        )
        .expect("room");
    }
    for _ in 0..50 {
        render(&net, ChannelLayout::MONO, &[512]);
        render(&graph, ChannelLayout::MONO, &[512]);
        assert_eq!(
            net_t.settings.beat().get().to_bits(),
            graph_t.settings.beat().get().to_bits()
        );
    }
    let graph_log = graph_log.lock().expect("log");
    let net_beats = net_beats.lock().expect("log");
    let beat = |f: usize| graph_log[f].1;
    // Linear from 3.0 at 1/24 000 beat a frame, loop armed or not.
    assert!((beat(19_999) - (3.0 + 19_999.0 / 24_000.0)).abs() < 1e-9);
    for f in [1_001usize, 5_000, 19_999] {
        let (w, fr) = net_beats[f];
        assert!(((w as f64 + fr as f64) - beat(f)).abs() < 1e-6, "frame {f}");
    }
    // Inside the loop from the seek: wraps at 2.0, 2 400 frames on.
    assert_eq!(beat(20_000), 1.9);
    assert!(beat(22_399) > 1.99);
    assert!(beat(22_401) < 1.01);
}

/// A declick stop at `At::Frame` inside a block stops the **transport** on
/// that frame; only the audio fades. The graph's `Env` reads the transport
/// stopped from the frame, a graph `At::Beat` command due after it in the
/// same block does not fire, and a `Net`'s clock holds on the same frame.
///
/// Mutation (run): leave `apply_outcome` out of `publish`'s
/// `DeclickStarted` arm (the old rule: the transport rolls until the fade
/// completes) → `Env` reads rolling after frame 600, the beat-0.03 note
/// fires, the Net clock keeps moving → fails.
#[test]
fn a_declick_stop_stops_the_transport_on_its_frame() {
    // Rolling from frame 256; a declick stop at frame 600, inside the block
    // 512..768.
    let transport = Transport::new(SR);
    let log = Arc::new(Mutex::new(Vec::new()));
    let (engine, _ed) = graph_engine(
        &transport,
        256,
        Gate {
            log: Some(Arc::clone(&log)),
        },
        1,
    );
    render(&engine, ChannelLayout::MONO, &[256]);
    let m = &transport.motion;
    m.schedule(At::Frame(Frame(256)), MotionEvent::Play)
        .expect("room");
    m.schedule(At::Frame(Frame(600)), MotionEvent::stop())
        .expect("room");
    render(&engine, ChannelLayout::MONO, &[256; 4]);
    let log = log.lock().expect("log");
    assert!(log[599].2, "rolling up to the stop");
    assert!(!log[600].2, "stopped on its frame, while the audio fades");
    assert_eq!(log[600].1, log[767].1, "the playhead holds");

    // A graph beat command after the stop's frame, in the same block: beat
    // 400/24 000 would be frame 656 had the transport rolled on.
    let t2 = Transport::new(SR);
    let notes = Arc::new(Mutex::new(Vec::new()));
    let (e, mut ed2) = graph_engine(&t2, 256, NoteLog(Arc::clone(&notes)), 1);
    render(&e, ChannelLayout::MONO, &[256]);
    ed2.collect();
    ed2.schedule(
        At::Beat(Beat(400.0 / FPB as f64)),
        EventIn {
            node: NodeKey(1),
            port: 0,
        },
        EventKind::Midi(Ump([0x2090_3c64, 0, 0, 0])),
    )
    .expect("room");
    t2.motion
        .schedule(At::Frame(Frame(256)), MotionEvent::Play)
        .expect("room");
    t2.motion
        .schedule(At::Frame(Frame(600)), MotionEvent::stop())
        .expect("room");
    render(&e, ChannelLayout::MONO, &[256; 8]);
    assert!(notes.lock().expect("log").is_empty(), "not reached");

    // A Net clock holds on the same frame.
    let net_t = Transport::new(SR);
    let beats = Arc::new(Mutex::new(Vec::new()));
    let net = net_engine(
        &net_t,
        Box::new(Surround {
            channels: 1,
            frame: 0,
        }),
        Some(Arc::clone(&beats)),
    );
    net_t
        .motion
        .schedule(At::Frame(Frame(256)), MotionEvent::Play)
        .expect("room");
    net_t
        .motion
        .schedule(At::Frame(Frame(600)), MotionEvent::stop())
        .expect("room");
    render(&net, ChannelLayout::MONO, &[256; 4]);
    let beats = beats.lock().expect("log");
    assert_ne!(beats[599], beats[600], "moving up to the stop");
    assert_eq!(beats[601], beats[1_000], "held from the stop");
}

// ---- limits, capacity, re-prepare --------------------------------------------

/// A graph engine refuses what its fold scratch cannot hold: a graph with
/// more than `MAX_ROOT_CHANNELS` global outputs at construction, and any
/// later commit that would widen past it; and an executor that is not the
/// editor's. Never silence.
///
/// Mutation (run): set no output limit in `with_graph_capacity`
/// (`max_global_outputs: usize::MAX`) → both refusals pass → fails. Skip the
/// pair check → the stranger's executor is accepted → fails.
#[test]
fn a_graph_engine_refuses_more_outputs_than_it_folds() {
    use tutti_core::{GraphEngineError, MAX_ROOT_CHANNELS};
    use tutti_graph::CommitError;

    let transport = Transport::new(SR);
    let (mut ed, exec) = Editor::new(prepare(256));
    ed.insert(NodeKey(1), "node", Clocked);
    // Ten outputs off a three-channel node's ports, repeated: a legal graph,
    // wider than the scratch.
    ed.spec_mut().topology.outputs = (0..10)
        .map(|c| {
            Source::Node(OutPort {
                node: NodeKey(1),
                port: c % 3,
            })
        })
        .collect();
    ed.commit().expect("no limits yet");
    assert!(matches!(
        Engine::with_graph(&transport, &mut ed, exec),
        Err(GraphEngineError::Limits(CommitError::TooManyOutputs {
            outputs: 10,
            ..
        }))
    ));

    let (engine, mut ed) = graph_engine(&transport, 256, Clocked, 3);
    render(&engine, ChannelLayout::STEREO, &[256]);
    ed.spec_mut().topology.outputs = (0..MAX_ROOT_CHANNELS as u16 + 1)
        .map(|c| {
            Source::Node(OutPort {
                node: NodeKey(1),
                port: c % 3,
            })
        })
        .collect();
    assert!(matches!(
        ed.commit(),
        Err(CommitError::TooManyOutputs {
            outputs: 9,
            limit: 8
        })
    ));

    let (mut ed, _exec) = Editor::new(prepare(256));
    let (_other, stranger) = Editor::new(prepare(256));
    assert_eq!(
        Engine::with_graph(&transport, &mut ed, stranger).err(),
        Some(GraphEngineError::NotAPair)
    );
}

/// Re-prepare the graph under a running engine, shrinking and then growing
/// `MaxBlock`: the engine adopts each new maximum on the block the resume
/// lands in (no stale split, no assertion in the callback), through both
/// `process` and `process_segment`; and a re-prepare past its block
/// capacity is refused on the control thread.
///
/// Mutation (run): read the block bound before `apply_pending` in `settle`
/// (the reviewed order) → the shrink hands the executor a 512-frame block
/// against a 256 maximum → panics → fails.
#[test]
fn re_preparing_under_the_engine_adopts_the_new_block_at_once() {
    use tutti_graph::CommitError;

    for segment in [false, true] {
        let transport = Transport::new(SR);
        let (mut ed, exec) = Editor::new(prepare(512));
        ed.insert(NodeKey(1), "node", Clocked);
        outputs(&mut ed, NodeKey(1), 3);
        ed.commit().expect("commits");
        let engine =
            Engine::with_graph_capacity(&transport, &mut ed, exec, Samples(2048)).expect("fits");
        assert_eq!(engine.graph_block_capacity(), Some(Samples(2048)));
        let layout = ChannelLayout::from_count(3);
        let go = |n: usize| {
            let mut buf = vec![0.0f32; n * 3];
            let mut out = InterleavedMut::new(&mut buf, layout);
            if segment {
                engine.process_segment(&mut out);
            } else {
                engine.process(&mut out);
            }
            buf
        };
        go(512);

        // Past the capacity: refused, nothing sent.
        assert!(matches!(
            ed.reprepare(prepare(4096)),
            Err(CommitError::BlockTooLong {
                max_block: 4096,
                limit: 2048
            })
        ));

        // Shrink to 256: the resume lands in the next 512-frame device
        // block, which is then rendered as two 256-frame graph blocks.
        ed.reprepare(prepare(256)).expect("within the capacity");
        go(512); // suspended: silence
        ed.collect(); // sends the resume
        let out = go(512);
        assert_eq!(out[1], 256.0, "block length after the shrink");
        assert_eq!(out[511 * 3 + 1], 256.0);

        // Grow to 2048: one 2048-frame graph block, not eight 256-frame ones.
        ed.reprepare(prepare(2048)).expect("at the capacity");
        go(512);
        ed.collect();
        let out = go(2048);
        assert_eq!(out[1], 2048.0, "block length after the growth");
    }
}

/// On the Net path, beats resolve with the tempo the net's clock runs at: a
/// tempo wiggle under the clock's hysteresis moves neither, so a beat-timed
/// stop at beat 10 lands on the same frame through both backends.
///
/// Mutation (run): report the raw `settings.tempo()` in `NetPieces::begin`
/// → the Net resolves beat 10 at 120.0005 BPM, two frames early → fails.
#[test]
fn a_tempo_wiggle_under_the_clock_hysteresis_moves_neither_backend() {
    let net_t = Transport::new(SR);
    let net_beats = Arc::new(Mutex::new(Vec::new()));
    let net = net_engine(
        &net_t,
        Box::new(Surround {
            channels: 1,
            frame: 0,
        }),
        Some(Arc::clone(&net_beats)),
    );
    let graph_t = Transport::new(SR);
    let graph_log = Arc::new(Mutex::new(Vec::new()));
    let (graph, _ed) = graph_engine(
        &graph_t,
        512,
        Gate {
            log: Some(Arc::clone(&graph_log)),
        },
        1,
    );
    for t in [&net_t, &graph_t] {
        t.settings.set_tempo(Bpm(120.0005));
        t.motion.try_send(MotionEvent::Play).expect("room");
        t.motion
            .schedule(At::Beat(Beat(10.0)), MotionEvent::stop_now())
            .expect("room");
    }
    for _ in 0..480 {
        render(&net, ChannelLayout::MONO, &[512]);
        render(&graph, ChannelLayout::MONO, &[512]);
    }
    let graph_log = graph_log.lock().expect("log");
    let net_beats = net_beats.lock().expect("log");
    // Beat 10 is frame 240 000 at 120 BPM; the playhead, accumulated frame
    // by frame, reaches it a millionth of a frame late or so, so the stop
    // may land one frame on. What must hold is that both backends stop on
    // the same frame.
    let stop = graph_log
        .iter()
        .position(|&(_, _, playing)| !playing)
        .expect("the graph stopped");
    assert!((240_000..=240_001).contains(&stop), "graph stops at {stop}");
    // The Net clock moves on its last rolling frame and holds from the stop.
    assert_ne!(
        net_beats[stop - 1],
        net_beats[stop],
        "net moving up to {stop}"
    );
    assert_eq!(
        net_beats[stop],
        net_beats[stop + 1],
        "net holds from {stop}"
    );
}

/// Ten minutes of 64-frame blocks through both backends, with a tempo
/// change and a loop partway: the published playheads, and the beat each
/// block starts on, stay bit-equal the whole way. Drift between the two
/// clocks would grow with the run, so a long one is where it shows.
///
/// Mutation (run): advance the graph clock by `beat_per_sample × frames` in
/// one step (closed form) instead of frame by frame → the playheads part
/// within the first blocks → fails.
#[test]
fn net_and_graph_agree_over_ten_minutes() {
    // Logs only each block's first beat on the graph side, and nothing on
    // the Net side but its published playhead: 450 000 blocks.
    struct FirstBeat(Arc<Mutex<f64>>);
    impl Node for FirstBeat {
        fn shape(&self) -> Shape {
            Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO).with_tail(Tail::Unbounded)
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
            *self.0.lock().expect("log") = cx.env.transport.beat.get();
            io.output(0).fill(0.0);
            Status::Modified
        }
        fn reset(&mut self) {}
    }
    let net_t = Transport::new(SR);
    // A Net with its clock, and no per-frame log.
    let mut n = Net::new(0, 1);
    let src = n.push(Box::new(Surround {
        channels: 1,
        frame: 0,
    }));
    n.connect_output(src, 0, 0);
    n.push(Box::new(TransportClock::new(net_t.clock_links(), SR)));
    n.set_sample_rate(SampleRate(SR));
    let backend = n.backend();
    Box::leak(Box::new(n));
    let net = Engine::new(net_t.motion.clone(), backend);

    let graph_t = Transport::new(SR);
    let first = Arc::new(Mutex::new(0.0));
    let (graph, _ed) = graph_engine(&graph_t, 64, FirstBeat(Arc::clone(&first)), 1);
    for t in [&net_t, &graph_t] {
        t.settings.set_tempo(Bpm(123.0));
        t.motion.try_send(MotionEvent::Play).expect("room");
        t.motion
            .schedule(
                At::Frame(Frame(48_000 * 200 + 17)),
                TransportCommand::Tempo(Bpm(91.5)),
            )
            .expect("room");
        t.motion
            .schedule(
                At::Frame(Frame(48_000 * 400 + 5)),
                TransportCommand::Loop(LoopRange::new(800.0, 816.0)),
            )
            .expect("room");
    }
    let blocks = 48_000 * 600 / 64;
    let mut net_buf = vec![0.0f32; 64];
    let mut graph_buf = vec![0.0f32; 64];
    for i in 0..blocks {
        let net_start = net_t.settings.beat();
        net.process(&mut InterleavedMut::new(&mut net_buf, ChannelLayout::MONO));
        graph.process(&mut InterleavedMut::new(
            &mut graph_buf,
            ChannelLayout::MONO,
        ));
        // The graph block started on the beat the Net's clock published at
        // the end of the previous block (its first emitted beat).
        if i > 0 {
            assert_eq!(
                first.lock().expect("log").to_bits(),
                net_start.get().to_bits(),
                "block {i}"
            );
        }
        assert_eq!(
            net_t.settings.beat().get().to_bits(),
            graph_t.settings.beat().get().to_bits(),
            "block {i}"
        );
    }
    // Not vacuous: it ran long (~715 beats by the loop's arming, ahead of
    // the loop, which it then plays into and holds).
    let end = graph_t.settings.beat().get();
    assert!(
        (800.0..816.0).contains(&end),
        "ends inside the loop, at {end}"
    );
}
