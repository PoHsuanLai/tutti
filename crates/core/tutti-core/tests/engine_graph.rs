//! `Engine` over the native graph (doc 013 Phase 2), and timestamped
//! transport commands through it.
//!
//! What is pinned here:
//!
//! - a graph rendered through `Engine` is the graph rendered by its executor
//!   directly, bit for bit;
//! - the engine folds a root to the device width with `fold_frame`, and its
//!   declick is the linear fade it is specified to be;
//! - a transport command at `At::Frame` / `At::Beat` lands on its frame in a
//!   graph node's `Env`;
//! - a graph `At::Beat` command scheduled against a timestamped start lands
//!   on its frame — the first engine-level case of the doc 013 §6 contract;
//! - the beat a graph node reads from `Env`, and the playhead the engine
//!   publishes, are the closed-form beat of each segment the commands cut.
//!
//! Until doc 013 Phase 3 PR 15 several of these compared the graph against
//! a `Net` rendered by the same engine. With the `Net` backend gone, each
//! comparison is pinned to what the `Net` was checked against: an analytic
//! figure (the fold matrix, the fade, the segment's closed form), computed
//! here without the engine's code.
//!
//! The allocation gate is in `rt_no_alloc_engine.rs`.

use std::sync::{Arc, Mutex};

mod support;

use support::{model_beats, Change, Segment};
use tutti_core::{
    At, AudioUnit, Beat, Bpm, BufferMut, BufferRef, ChannelLayout, Engine, FadeOut, Frame,
    InterleavedMut, LoopRange, MotionEvent, SampleRate, Samples, Signal, SignalFrame, Tail, Then,
    Transport, TransportCommand,
};
use tutti_graph::{
    Cx, Editor, EventIn, EventKind, Executor, IntoNode, Io, Node, Prepare, Shape, Status, Ump,
    Unforkable,
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
                if t.playing { 1.0 } else { 0.0 } + t.beat().get().fract() as f32 * 0.5;
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
                log.push((env.frame_at(k).get(), t.beat().get(), t.playing));
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

// ---- a legacy unit (through `Legacy`) ---------------------------------------

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

/// A graph engine over `transport` with one node at key 1, its outputs wired
/// to the globals. The editor is returned, for scheduling.
fn graph_engine(
    transport: &Transport,
    max_block: usize,
    node: impl IntoNode<Controls = ()>,
    width: u16,
) -> (Engine, Editor) {
    let (mut ed, exec) = Editor::new(prepare(max_block));
    ed.insert(NodeKey(1), "node", node);
    outputs(&mut ed, NodeKey(1), width);
    ed.commit().expect("commits");
    let engine = Engine::new(transport, &mut ed, exec).expect("within the limits");
    (engine, ed)
}

// ---- the render ----------------------------------------------------------

/// A graph rendered through `Engine` is the graph rendered by its executor,
/// bit for bit: whole device blocks, one executor block each (a device block
/// past the prepared maximum as consecutive maximum-sized ones), the
/// executor's own frame clock, the transport as it stands, and a fold that is
/// the identity at equal width.
///
/// Mutation (run): cap the engine's block at 64 frames
/// (`block_bound`) → channel 1 reads 64 → fails.
#[test]
fn graph_engine_render_is_bit_identical_to_the_executor() {
    let blocks = [64usize, 256, 100, 1024, 1, 512];
    let transport = Transport::new(SR);
    let (engine, _ed) = graph_engine(&transport, 512, Unforkable(Clocked), 3);
    let got = render(&engine, ChannelLayout::from_count(3), &blocks);

    let (mut ed, mut exec): (Editor, Executor) = Editor::new(prepare(512));
    ed.insert(NodeKey(1), "node", Unforkable(Clocked));
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

/// The engine folds a root to the device width, and its declick is a
/// linear fade: the same source (`Surround`, through `Legacy`), stereo and
/// 5.1, to stereo and 5.1 devices, rolling and then stopped with a declick
/// mid-run.
///
/// Until doc 013 PR 15 this compared the graph bit for bit against a `Net`
/// rendered by the same engine. The oracle is now what that comparison
/// stood for, computed here: each frame of the source (deterministic and
/// libm-free), folded by `fold_frame` (the ITU/Dolby matrices, plain
/// arithmetic), times the declick gain — 1 before the stop, zero on the
/// stop's frame (an untimed stop has no lead) and `k / 480` for `k` frames
/// after it, up to 1. Bit-equal wherever the gain is 1; within `f32`
/// rounding of the ramp inside it: the fader accumulates `1/480` a frame in
/// `f32`, at most 480 roundings of half an ulp of 1 (under 3e-5, relative).
///
/// Mutation (run): copy channel `c` straight across instead of `fold_frame`
/// in the graph render → the 6→2 case fails. Skip the fader's `apply` → the
/// stop's frame is not zero → fails.
#[test]
fn fold_and_declick_are_the_fold_matrix_and_a_linear_fade() {
    let blocks = [256usize, 256, 300, 64, 128, 512, 256];
    let total: usize = blocks.iter().sum();
    // The untimed stop lands on the first frame of the fourth block.
    let stop = 256 + 256 + 300;
    for (src, device) in [(2usize, 2u16), (6, 2), (6, 6), (2, 6)] {
        let layout = ChannelLayout::from_count(device);
        let ch = device as usize;
        let transport = Transport::new(SR);
        let (engine, _ed) = graph_engine(
            &transport,
            512,
            tutti_graph::Legacy::new(Surround {
                channels: src,
                frame: 0,
            }),
            src as u16,
        );
        transport.motion.try_send(MotionEvent::Play).expect("room");
        let mut got = render(&engine, layout, &blocks[..3]);
        transport
            .motion
            .try_send(MotionEvent::stop())
            .expect("room");
        got.extend(render(&engine, layout, &blocks[3..]));
        assert!(transport.motion.is_stopped());

        for f in 0..total {
            let frame: Vec<f32> = (0..src).map(|c| value(f as u64, c)).collect();
            let mut folded = vec![0.0f32; ch];
            tutti_core::fold_frame(&frame, &mut folded);
            let at = &got[f * ch..(f + 1) * ch];
            if f < stop || f >= stop + 480 {
                assert_eq!(bits(at), bits(&folded), "{src} → {device}, frame {f}");
            } else {
                let gain = (f - stop) as f32 / 480.0;
                for (c, (&g, &x)) in at.iter().zip(&folded).enumerate() {
                    assert!(
                        (g - x * gain).abs() <= x.abs() * 3e-5,
                        "{src} → {device}, frame {f} channel {c}: {g}, want {x} × {gain}"
                    );
                }
            }
        }
        // Not vacuous: sound before the stop, zero on its frame.
        assert!(got[..stop * ch].iter().any(|&x| x != 0.0));
        assert!(got[stop * ch..(stop + 1) * ch].iter().all(|&x| x == 0.0));
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
    let (engine, _ed) = graph_engine(&transport, 256, Unforkable(Gate { log: None }), 1);
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

/// Beat-timed seek and stop land on the frame their beat resolves to:
/// rolling from 0 at 120 BPM, a seek at beat 1 is frame 24 000, and a stop at
/// beat 10.5 (half a beat after the seek to 10) is frame 36 000.
///
/// The seek's beat falls on a block's first frame, where the engine's
/// playhead is exactly beat 1 (it counts frames and derives the beat, doc
/// 013 §6): it must land there, on that frame, without counting as late.
///
/// Mutation (run): land every due command at its piece's first frame
/// (`at = cursor` in the walk) → the stop lands at its block's start →
/// fails. Hand the executor `TransportChanges::NONE` → fails. Drop
/// `schedule.release` → `scheduled_outstanding` stays 2 → fails. (Dropping
/// the behind-side rounding tolerance in `Env::beat_due` failed this test
/// while the playhead accumulated and sat ~1e-12 beat past beat 1; with the
/// playhead exact it no longer does, and tutti-graph's
/// `a_beat_a_rounding_error_behind_is_the_first_frame` pins the tolerance.)
#[test]
fn beat_timed_seek_and_stop_land_on_their_frames() {
    let transport = Transport::new(SR);
    let log = Arc::new(Mutex::new(Vec::new()));
    let (engine, _ed) = graph_engine(
        &transport,
        700,
        Unforkable(Gate {
            log: Some(Arc::clone(&log)),
        }),
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
            log.push((env.frame_at(k).get(), t.beat().get(), t.playing));
            io.output(0)[k.index()] = 1.0;
        }
        Status::Modified
    }
    fn reset(&mut self) {}
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

/// Run `script` against a DC source, rolling from beat 0, in 256-frame
/// blocks for `blocks` blocks; `script(motion, i)` is called before block
/// `i`. Returns the output — the declick gain, read straight off it — and
/// the transport per frame as `(beat, playing)`, from the graph's `Env`.
///
/// (Until doc 013 PR 15 it also ran a `Net` engine, and every assertion
/// held for both; the assertions were analytic, so they stand alone.)
#[allow(clippy::type_complexity)]
fn dc_run(
    blocks: usize,
    script: impl Fn(&tutti_core::MotionFsm, usize),
) -> (Vec<f32>, Vec<(f64, bool)>) {
    let transport = Transport::new(SR);
    let log = Arc::new(Mutex::new(Vec::new()));
    let (engine, _ed) = graph_engine(&transport, 256, Unforkable(DcLog(Arc::clone(&log))), 1);
    let mut out = Vec::new();
    transport.motion.try_send(MotionEvent::Play).expect("room");
    for i in 0..blocks {
        script(&transport.motion, i);
        out.extend(render(&engine, ChannelLayout::MONO, &[256]));
    }
    let t = log
        .lock()
        .expect("log")
        .iter()
        .map(|&(_, b, p)| (b, p))
        .collect();
    (out, t)
}

/// A seek to beat 10 at `At::Frame(1000)`, mid-block, with a declick: the
/// old position's audio fades out over the 480 frames **before** the seek
/// (from 520), reaching zero exactly on frame 1 000, where the transport
/// jumps; the new position's audio fades in from there. The gain never moves
/// by more than one fade step a frame.
///
/// Mutation (run): give no lead (`Gain::Aim(None)` at every walk step) →
/// the gain steps from 1 to 0 at 1 000 → fails. Snap the gain back to 1
/// after the jump (no fade-in) → fails.
#[test]
fn a_timed_declicked_seek_fades_out_before_and_in_after_its_frame() {
    let (out, t) = dc_run(12, |m, i| {
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
    assert_eq!(out[519], 1.0, "full level before the lead");
    assert!(out[520] < 1.0, "the fade-out starts 480 ahead");
    assert_eq!(out[1_000], 0.0, "zero on the command frame");
    assert!(out[1_001] > 0.0, "the fade-in starts there");
    assert_eq!(out[1_480], 1.0, "back to full level");
    assert!(max_step(&out, &[]) <= FADE_STEP, "continuous");
    // The transport jumps on the command frame.
    assert!(t[999].0 < 1.0, "old position up to 999");
    assert_eq!(t[1_000].0, 10.0, "beat 10 from frame 1 000");
    assert!(t[1_000].1, "still rolling");
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
    let (out, t) = dc_run(12, |m, i| {
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
    assert_eq!(out[1_023], 1.0);
    assert!(out[1_024] < 1.0, "fading from the first frame it is seen");
    assert_eq!(out[1_224], 0.0, "zero on the command frame");
    assert!(max_step(&out, &[]) <= 1.0 / 200.0 + 1e-5, "continuous");
    assert_eq!(t[1_224].0, 10.0, "jumps on its frame");
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
    let (out, t) = dc_run(12, |m, i| {
        if i == 0 {
            m.schedule(At::Frame(Frame(1_000)), MotionEvent::stop())
                .expect("room");
        }
    });
    assert!(out[520] < 1.0 && out[519] == 1.0);
    assert_eq!(out[1_000], 0.0);
    assert!(max_step(&out, &[]) <= FADE_STEP, "continuous");
    assert!(t[999].1, "rolling up to the stop");
    assert!(!t[1_000].1, "stopped on its frame");
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
    let (out, t) = dc_run(8, |m, i| {
        if i == 3 {
            m.try_send(MotionEvent::Locate {
                beat: Beat(10.0),
                fade: FadeOut::Declick,
                then: Then::Keep,
            })
            .expect("room");
        }
    });
    let jump = 3 * 256;
    assert_eq!(out[jump - 1], 1.0, "no lead, full level");
    assert_eq!(out[jump], 0.0, "zero on the jump frame");
    assert!(max_step(&out, &[jump]) <= FADE_STEP, "fade-in continuous");
    assert_eq!(out[jump + 480], 1.0);
    assert_eq!(t[jump].0, 10.0, "jumps on the block's first frame");
}

/// A command whose frame is already past lands at the next block's first
/// frame and is counted late, never dropped.
///
/// Mutation (run): drop `schedule.count_late()` → fails.
#[test]
fn a_past_frame_lands_at_once_and_is_counted() {
    let transport = Transport::new(SR);
    let (engine, _ed) = graph_engine(&transport, 256, Unforkable(Gate { log: None }), 1);
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
        Unforkable(Gate {
            log: Some(Arc::clone(&log)),
        }),
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
    let (engine, mut ed) =
        graph_engine(&transport, 256, Unforkable(NoteLog(Arc::clone(&notes))), 1);
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

/// The beat a graph node reads from `Env`, through a start, a tempo change,
/// a loop, a seek, timed commands and an untimed tempo edit between blocks,
/// is the closed-form beat of each segment those cut, on every frame; the
/// playhead the engine publishes after each block is the model's beat at
/// the block's end, and the next block starts on it, to the bit.
///
/// Until doc 013 PR 15 the oracle was a `Net`'s `TransportClock` rendered by
/// the same engine (bit-equal at block starts and cuts, within 1e-7
/// between, through its `f32` ports). The model below is what that clock
/// was pinned to in `clock.rs`; it is exact up to the rebasing at each
/// segment's origin, so the tolerance is 1e-9 beat.
///
/// Mutation (run): skip the loop wrap in `FrameClock::advance` → fails after
/// the loop arms. Hand the executor `TransportChanges::NONE` → the
/// per-frame beats differ after a cut → fails. Land every due command at
/// its piece's first frame → fails.
#[test]
fn env_beats_are_the_closed_form_of_each_segment() {
    let transport = Transport::new(SR);
    let log = Arc::new(Mutex::new(Vec::new()));
    let (engine, _ed) = graph_engine(
        &transport,
        512,
        Unforkable(Gate {
            log: Some(Arc::clone(&log)),
        }),
        1,
    );
    let blocks = [512usize, 300, 512, 77, 512, 512, 400, 512, 512];
    let m = &transport.motion;
    m.try_send(MotionEvent::Play).expect("room");
    m.schedule(At::Frame(Frame(1_000)), TransportCommand::Tempo(Bpm(97.0)))
        .expect("room");
    // Armed with the playhead inside it (beat ~0.065): one wrap at ~2 140,
    // disarmed before the next.
    m.schedule(
        At::Frame(Frame(1_700)),
        TransportCommand::Loop(LoopRange::new(0.02, 0.08)),
    )
    .expect("room");
    m.schedule(At::Frame(Frame(2_900)), TransportCommand::Loop(None))
        .expect("room");
    m.schedule(
        At::Frame(Frame(3_100)),
        MotionEvent::Locate {
            beat: Beat(3.25),
            fade: FadeOut::Immediate,
            then: Then::Keep,
        },
    )
    .expect("room");
    // Block 6 starts at 2 425: an untimed edit there, as a UI makes one,
    // reaches the block's first frame.
    let untimed_at: usize = blocks[..6].iter().sum();
    let mut published = Vec::new();
    for (i, &n) in blocks.iter().enumerate() {
        if i == 6 {
            transport.settings.set_tempo(Bpm(133.0));
        }
        render(&engine, ChannelLayout::MONO, &[n]);
        published.push(transport.settings.beat().get());
    }
    let frames: usize = blocks.iter().sum();
    let model = model_beats(
        SR,
        120.0,
        &[
            (1_000, Change::Tempo(97.0)),
            (1_700, Change::Loop(Some((0.02, 0.08)))),
            (untimed_at as u64, Change::Tempo(133.0)),
            (2_900, Change::Loop(None)),
            (3_100, Change::Seek(3.25)),
        ],
        frames as u64,
    );
    let log = log.lock().expect("log");
    assert_eq!(log.len(), frames);
    for (f, &(_, beat, playing)) in log.iter().enumerate() {
        assert!(playing, "frame {f}");
        assert!(
            (beat - model[f]).abs() < 1e-9,
            "frame {f}: graph {beat}, model {}",
            model[f]
        );
    }
    let mut end = 0;
    for (i, (&n, &p)) in blocks.iter().zip(&published).enumerate() {
        end += n;
        assert!(
            (p - model[end]).abs() < 1e-9,
            "published after block {i}: {p}, model {}",
            model[end]
        );
        if end < frames {
            assert_eq!(
                log[end].1.to_bits(),
                p.to_bits(),
                "block {} starts on the published playhead",
                i + 1
            );
        }
    }
    // Not vacuous: the seek and the loop happened.
    assert!(log.iter().any(|&(_, b, _)| b >= 3.25));
    let looped = &log[1_700..2_900];
    assert!(looped.iter().all(|&(_, b, _)| b < 0.08));
    assert!(looped.windows(2).any(|w| w[1].1 < w[0].1), "it wrapped");
}

/// A loop armed while the playhead is past its end does not jump: playback
/// runs on, in the graph's `Env` and in the playhead the engine publishes,
/// until a seek puts the playhead inside the loop, and from then it wraps
/// (doc 013's decision, the common DAW behaviour).
///
/// Until doc 013 PR 15 a `Net`'s clock ran beside the graph and the two
/// published playheads were compared to the bit; both were already pinned
/// to the linear beat here, which now also bounds the published playhead
/// after every block.
///
/// Mutation (run): drop the armed-behind guard in `FrameClock::advance`
/// (wrap whether or not the playhead was before the end) → the clock jumps
/// into the loop on the frame after it is armed, the published playhead is
/// inside [1, 2) → fails. (`LoopRange::advance`, which this note used to
/// name, is the offline timeline's rule; the live clock's is that guard.)
#[test]
fn a_loop_armed_behind_the_playhead_does_not_jump() {
    let transport = Transport::new(SR);
    let log = Arc::new(Mutex::new(Vec::new()));
    let (engine, _ed) = graph_engine(
        &transport,
        512,
        Unforkable(Gate {
            log: Some(Arc::clone(&log)),
        }),
        1,
    );
    let m = &transport.motion;
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
    for i in 0..50u64 {
        render(&engine, ChannelLayout::MONO, &[512]);
        let end = 512 * (i + 1);
        if end < 20_000 {
            // The published playhead is linear too, never inside the loop.
            let want = 3.0 + end as f64 / 24_000.0;
            let got = transport.settings.beat().get();
            assert!((got - want).abs() < 1e-9, "after block {i}: {got}");
        }
    }
    let graph_log = log.lock().expect("log");
    let beat = |f: usize| graph_log[f].1;
    // Linear from 3.0 at 1/24 000 beat a frame, loop armed or not.
    for f in [1_001usize, 5_000, 19_999] {
        let want = 3.0 + f as f64 / 24_000.0;
        assert!((beat(f) - want).abs() < 1e-9, "frame {f}: {}", beat(f));
    }
    // Inside the loop from the seek: wraps at 2.0, 2 400 frames on.
    assert_eq!(beat(20_000), 1.9);
    assert!(beat(22_399) > 1.99);
    assert!(beat(22_401) < 1.01);
}

/// A declick stop at `At::Frame` inside a block stops the **transport** on
/// that frame; only the audio fades. The graph's `Env` reads the transport
/// stopped from the frame, its beat holds, and a graph `At::Beat` command
/// due after it in the same block does not fire. (Until doc 013 PR 15 a
/// `Net`'s clock was also seen to hold on the same frame; the held `Env`
/// beat is the same assertion on the one clock left.)
///
/// Mutation (run): leave `apply_outcome` out of `publish`'s
/// `DeclickStarted` arm (the old rule: the transport rolls until the fade
/// completes) → `Env` reads rolling after frame 600, the beat-0.03 note
/// fires → fails.
#[test]
fn a_declick_stop_stops_the_transport_on_its_frame() {
    // Rolling from frame 256; a declick stop at frame 600, inside the block
    // 512..768.
    let transport = Transport::new(SR);
    let log = Arc::new(Mutex::new(Vec::new()));
    let (engine, _ed) = graph_engine(
        &transport,
        256,
        Unforkable(Gate {
            log: Some(Arc::clone(&log)),
        }),
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
    let (e, mut ed2) = graph_engine(&t2, 256, Unforkable(NoteLog(Arc::clone(&notes))), 1);
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
}

// ---- limits, capacity, re-prepare --------------------------------------------

/// A graph engine refuses what its fold scratch cannot hold: a graph with
/// more than `MAX_ROOT_CHANNELS` global outputs at construction, and any
/// later commit that would widen past it; and an executor that is not the
/// editor's. Never silence.
///
/// Mutation (run): set no output limit in `with_capacity`
/// (`max_global_outputs: usize::MAX`) → both refusals pass → fails. Skip the
/// pair check → the stranger's executor is accepted → fails.
#[test]
fn a_graph_engine_refuses_more_outputs_than_it_folds() {
    use tutti_core::{GraphEngineError, MAX_ROOT_CHANNELS};
    use tutti_graph::CommitError;

    let transport = Transport::new(SR);
    let (mut ed, exec) = Editor::new(prepare(256));
    ed.insert(NodeKey(1), "node", Unforkable(Clocked));
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
        Engine::new(&transport, &mut ed, exec),
        Err(GraphEngineError::Limits(CommitError::TooManyOutputs {
            outputs: 10,
            ..
        }))
    ));

    let (engine, mut ed) = graph_engine(&transport, 256, Unforkable(Clocked), 3);
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
        Engine::new(&transport, &mut ed, stranger).err(),
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
        ed.insert(NodeKey(1), "node", Unforkable(Clocked));
        outputs(&mut ed, NodeKey(1), 3);
        ed.commit().expect("commits");
        let engine = Engine::with_capacity(&transport, &mut ed, exec, Samples(2048)).expect("fits");
        assert_eq!(engine.block_capacity(), Samples(2048));
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

/// Beats resolve with the tempo the engine's clock runs at: a tempo wiggle
/// under the clock's hysteresis does not move it, so a beat-timed stop at
/// beat 10 lands on beat 10's frame at the tempo in force (120 BPM), not
/// at the 120.0005 BPM asked. (Until doc 013 PR 15 the same stop was also
/// pinned through a `Net`, whose side resolved beats separately; the graph
/// side's analytic frame is what is left.)
///
/// Mutation (run): take the raw asked tempo in `TransportClock::take_tempo`
/// (no hysteresis) → the clock runs at 120.0005 BPM and the stop's frame
/// is not on beat 10 → fails.
#[test]
fn a_tempo_wiggle_under_the_clock_hysteresis_does_not_move_the_beat() {
    let transport = Transport::new(SR);
    let log = Arc::new(Mutex::new(Vec::new()));
    let (engine, _ed) = graph_engine(
        &transport,
        512,
        Unforkable(Gate {
            log: Some(Arc::clone(&log)),
        }),
        1,
    );
    transport.settings.set_tempo(Bpm(120.0005));
    transport.motion.try_send(MotionEvent::Play).expect("room");
    transport
        .motion
        .schedule(At::Beat(Beat(10.0)), MotionEvent::stop_now())
        .expect("room");
    for _ in 0..480 {
        render(&engine, ChannelLayout::MONO, &[512]);
    }
    let log = log.lock().expect("log");
    // Beat 10 is frame 240 000 at 120 BPM (the 120.0005 asked is inside the
    // clock's hysteresis). The playhead counts frames and derives the beat,
    // so it is exactly beat 10 there and the stop lands on that frame. (It
    // accumulated once, reached beat 10 a millionth of a frame late, and the
    // stop was allowed one frame on.)
    let stop = log
        .iter()
        .position(|&(_, _, playing)| !playing)
        .expect("the graph stopped");
    assert_eq!(stop, 240_000, "stops on beat 10's frame");
    assert_eq!(log[stop].1, 10.0, "and holds beat 10");
    let in_force = transport
        .settings
        .tempo_in_force
        .load(std::sync::atomic::Ordering::Acquire);
    assert_eq!(in_force, 120.0, "the tempo in force");
}

/// Ten minutes of 64-frame blocks, with a tempo change and a loop partway:
/// the playhead the engine publishes after every block is the closed form
/// of its segment (bit-equal until the loop first wraps, and within 1e-9
/// beat, modulo the loop, after), and every block starts on the beat the
/// last one published. Drift would grow with the run, so a long one is
/// where it shows.
///
/// Until doc 013 PR 15 the oracle was a `Net`'s clock rendered beside the
/// graph by the same engine, compared to the bit; that clock is `FrameClock`
/// in closed form, which the model here writes out (`Segment::at` is
/// `TimelineSegment::beat_at`'s arithmetic, IEEE-exact on every target: no
/// libm).
///
/// Mutation (run): advance the engine's clock by accumulating
/// (`beat + beats_per_sample × frames`, a new segment each block) → the
/// playhead parts from the closed form within the first blocks → fails.
#[test]
fn the_published_playhead_is_the_closed_form_over_ten_minutes() {
    // Logs only each block's first beat: 450 000 blocks.
    struct FirstBeat(Arc<Mutex<f64>>);
    impl Node for FirstBeat {
        fn shape(&self) -> Shape {
            Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO).with_tail(Tail::Unbounded)
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
            *self.0.lock().expect("log") = cx.env.transport.beat().get();
            io.output(0).fill(0.0);
            Status::Modified
        }
        fn reset(&mut self) {}
    }
    const TEMPO_AT: u64 = 48_000 * 200 + 17;
    const LOOP_AT: u64 = 48_000 * 400 + 5;
    let (loop_start, loop_end) = (800.0, 816.0);

    let transport = Transport::new(SR);
    let first = Arc::new(Mutex::new(0.0));
    let (engine, _ed) = graph_engine(&transport, 64, Unforkable(FirstBeat(Arc::clone(&first))), 1);
    transport.settings.set_tempo(Bpm(123.0));
    transport.motion.try_send(MotionEvent::Play).expect("room");
    transport
        .motion
        .schedule(
            At::Frame(Frame(TEMPO_AT)),
            TransportCommand::Tempo(Bpm(91.5)),
        )
        .expect("room");
    transport
        .motion
        .schedule(
            At::Frame(Frame(LOOP_AT)),
            TransportCommand::Loop(LoopRange::new(loop_start, loop_end)),
        )
        .expect("room");

    let a = Segment {
        frame: 0,
        beat: 0.0,
        bpm: 123.0,
        rate: SR,
    };
    let b = Segment {
        frame: TEMPO_AT,
        beat: a.at(TEMPO_AT),
        bpm: 91.5,
        rate: SR,
    };
    // Unwrapped: the loop only folds it.
    let linear = |f: u64| if f < TEMPO_AT { a.at(f) } else { b.at(f) };
    let len = loop_end - loop_start;
    let blocks = 48_000 * 600 / 64;
    let mut buf = vec![0.0f32; 64];
    let mut wrapped = false;
    let mut last = 0.0f64;
    for i in 0..blocks as u64 {
        engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::MONO));
        if i > 0 {
            assert_eq!(
                first.lock().expect("log").to_bits(),
                last.to_bits(),
                "block {i} starts on the published playhead"
            );
        }
        let end = 64 * (i + 1);
        let got = transport.settings.beat().get();
        let want = linear(end);
        wrapped |= end > LOOP_AT && want >= loop_end;
        if !wrapped {
            assert_eq!(got.to_bits(), want.to_bits(), "block {i}: {got} vs {want}");
        } else {
            let folded = loop_start + (want - loop_start).rem_euclid(len);
            let d = (got - folded).abs();
            assert!(d.min(len - d) < 1e-9, "block {i}: {got} vs {folded}");
        }
        last = got;
    }
    // Not vacuous: it ran long (~715 beats by the loop's arming, ahead of
    // the loop, which it then plays into and holds).
    assert!(wrapped, "the loop wrapped");
    let end = transport.settings.beat().get();
    assert!(
        (loop_start..loop_end).contains(&end),
        "ends inside the loop, at {end}"
    );
}

/// Two declicked commands close together: the second's target is set on the
/// first's jump frame, so the gain stays at zero from the first frame to the
/// second and fades in from there, with no step anywhere. Three cases: a
/// seek at 1 000 then a stop at 1 020; a seek at 900 then a seek at 1 000;
/// an untimed seek (no lead, the accepted step on its frame) then a timed
/// one 100 frames on, inside its fade window.
///
/// Mutation (run): clear the aim after the inner loop on a jump (the
/// reviewed order: `if jump { self.aim = None }`) → the aim pushed after the
/// jump at the same offset is lost, the gain rises and is forced back to 0
/// at the second command → a step (0.04 and 0.2 in the review's probes) →
/// fails.
#[test]
fn two_declicked_commands_close_together_stay_continuous() {
    let seek = |beat: f64| MotionEvent::Locate {
        beat: Beat(beat),
        fade: FadeOut::Declick,
        then: Then::Keep,
    };
    // (first frame, second frame, whether the second is a stop)
    for (a, b, stop) in [(1_000u64, 1_020u64, true), (900, 1_000, false)] {
        let (out, t) = dc_run(12, |m, i| {
            if i == 0 {
                m.schedule(At::Frame(Frame(a)), seek(10.0)).expect("room");
                let second: TransportCommand = if stop {
                    MotionEvent::stop().into()
                } else {
                    seek(20.0).into()
                };
                m.schedule(At::Frame(Frame(b)), second).expect("room");
            }
        });
        let (a, b) = (a as usize, b as usize);
        assert_eq!(out[a], 0.0, "{a}/{b}");
        assert!(out[a..=b].iter().all(|&g| g == 0.0), "held at zero");
        assert!(out[b + 1] > 0.0, "fades in from the second");
        assert!(max_step(&out, &[]) <= FADE_STEP, "{a}/{b}: continuous");
        assert_eq!(t[a].0, 10.0, "the first lands on its frame");
        if stop {
            assert!(!t[b].1, "stopped on its frame");
        } else {
            assert_eq!(t[b].0, 20.0, "the second lands on its frame");
        }
    }

    // Untimed at block 3's first frame (768), then timed at 868.
    let (out, t) = dc_run(8, |m, i| {
        if i == 3 {
            m.try_send(seek(10.0)).expect("room");
            m.schedule(At::Frame(Frame(868)), seek(20.0)).expect("room");
        }
    });
    assert!(out[768..=868].iter().all(|&g| g == 0.0), "held at zero");
    assert!(
        max_step(&out, &[768]) <= FADE_STEP,
        "continuous past the jump"
    );
    assert_eq!(t[768].0, 10.0);
    assert_eq!(t[868].0, 20.0);
}

// ---- a rate change under the engine (a device restart) -------------------

/// The rate before the change, and the one after: 44.1 kHz to 48 kHz, a
/// device restart's commonest case.
const OLD_SR: f64 = 44_100.0;
const NEW_SR: f64 = 48_000.0;

/// **A re-prepare to a new rate keeps the beat and a frame-timed transport
/// command on wall-clock time, through the graph engine.**
///
/// Rolling at 120 BPM from beat 0, half a second at 44.1 kHz (22 050
/// frames), then a re-prepare to 48 kHz: the executor rescales its frame
/// clock to 24 000 on the block the first commit lands, renders that block
/// as silence, and resumes after `collect`. Two beats a second means the
/// beat at any logged frame `f` is `f / 24 000` from then on — so the beat
/// is continuous across the silent block, and a stop scheduled at
/// `Frame(44 100)` (one second at the old rate) lands on frame 48 000, one
/// second at the new one.
///
/// Mutations (run):
/// - `GraphRender::settle` following the rate at the resume only (reading
///   `exec.prepare()`, as before this change) → the clock steps the silent
///   block at the old rate, and every later beat is ~0.0018 of a beat
///   early → fails;
/// - dropping `schedule.rescale` there → the stop lands on executor frame
///   44 100 → fails.
#[test]
fn a_re_prepare_keeps_the_beat_and_a_frame_command_on_wall_clock_time() {
    let transport = Transport::new(OLD_SR);
    let log = Arc::new(Mutex::new(Vec::new()));
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(OLD_SR), Samples(512)));
    ed.insert(
        NodeKey(1),
        "node",
        Unforkable(Gate {
            log: Some(Arc::clone(&log)),
        }),
    );
    outputs(&mut ed, NodeKey(1), 1);
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");
    transport.motion.try_send(MotionEvent::Play).expect("room");
    transport
        .motion
        .schedule(At::Frame(Frame(44_100)), MotionEvent::stop_now())
        .expect("room");

    render(&engine, ChannelLayout::MONO, &[441; 50]);
    ed.reprepare(Prepare::new(SampleRate(NEW_SR), Samples(512)))
        .expect("re-prepares");
    // The suspended block: silence, and the rate moves here.
    render(&engine, ChannelLayout::MONO, &[480]);
    ed.collect();
    render(&engine, ChannelLayout::MONO, &[480; 60]);

    let log = log.lock().expect("log");
    let (before, after): (Vec<_>, Vec<_>) = log.iter().partition(|e| e.0 < 22_050);
    assert_eq!(before.len(), 22_050, "half a second at the old rate");
    for &&(f, beat, _) in &before {
        assert!(
            (beat - f as f64 / 22_050.0).abs() < 1e-9,
            "frame {f}: {beat}"
        );
    }
    // Resumed after the silent block, at the rescaled clock.
    assert_eq!(after.first().map(|e| e.0), Some(24_000 + 480));
    for &&(f, beat, playing) in &after {
        if playing {
            assert!(
                (beat - f as f64 / 24_000.0).abs() < 1e-9,
                "frame {f} at 48 kHz: beat {beat}, wall clock says {}",
                f as f64 / 24_000.0
            );
        }
    }
    let stop = after
        .iter()
        .find(|e| !e.2)
        .map(|e| e.0)
        .expect("the stop landed");
    assert_eq!(stop, 48_000, "one second, at the new rate");
    assert_eq!(transport.motion.late_commands(), 0);
}

// ---- one playhead writer, and its segment generation ----------------------

/// **One engine per transport.** While an engine drives a transport's
/// clock, a second engine over it — or over a clone of it, which shares the
/// playhead — is refused with `PlayheadClaimed`, as is a bare second clock
/// (`Transport::clock_links`); once the engine is dropped the transport
/// hands the claim out again. Two engines would both consume every seek
/// and both write the playhead.
///
/// Mutation (run): `PlayheadClaim::take` always succeeding → the second
/// engine builds → fails. Mutation (run): `PlayheadClaim`'s `Drop` not
/// giving the claim back → the engine after the drop is refused → fails.
/// Mutation (run): `Engine::with_capacity` building its clock from severed
/// links (no claim) → the second engine builds → fails.
#[test]
fn a_transport_has_one_playhead_writer() {
    use tutti_core::transport::PlayheadClaimed;
    use tutti_core::GraphEngineError;

    let transport = Transport::new(SR);
    let (engine, _ed) = graph_engine(&transport, 256, Unforkable(Clocked), 3);

    let (mut ed, exec) = Editor::new(prepare(256));
    let twin = transport.clone();
    assert_eq!(
        Engine::new(&twin, &mut ed, exec).err(),
        Some(GraphEngineError::PlayheadClaimed(PlayheadClaimed)),
        "a clone shares the playhead, so it shares the claim"
    );
    assert_eq!(transport.clock_links().err(), Some(PlayheadClaimed));

    drop(engine);
    let (mut ed, exec) = Editor::new(prepare(256));
    let again = Engine::new(&twin, &mut ed, exec).expect("the first engine is gone");
    render(&again, ChannelLayout::STEREO, &[64]);
}

/// **A seek to the beat the playhead already stands on moves the live
/// segment generation on**, and so does a play start; a block that only
/// rolls does not. What the sampler's seat keys on, so it re-seats through
/// a jump its beat cannot show.
///
/// Mutation (run): `TransportClock::publish_position` not storing the
/// generation → it stays 0 → fails. Mutation (run): `note_rolling` not
/// marking the start → the play start leaves it → fails.
#[test]
fn a_seek_to_the_same_beat_moves_the_live_generation() {
    use tutti_core::Timeline;

    let transport = Transport::new(SR);
    let (engine, _ed) = graph_engine(&transport, 256, Unforkable(Clocked), 3);
    render(&engine, ChannelLayout::STEREO, &[256]);
    let (beat, generation) = (transport.beat(), transport.segment_generation());

    // Stopped, so the beat holds: a seek to it jumps nowhere a beat shows.
    transport
        .motion
        .try_send(MotionEvent::Locate {
            beat,
            fade: FadeOut::Immediate,
            then: Then::Keep,
        })
        .expect("room");
    render(&engine, ChannelLayout::STEREO, &[256]);
    assert_eq!(transport.beat(), beat, "the same beat");
    let sought = transport.segment_generation();
    assert!(
        sought > generation,
        "a new segment: {generation} → {sought}"
    );

    render(&engine, ChannelLayout::STEREO, &[256]);
    assert_eq!(transport.segment_generation(), sought, "holding is no jump");

    transport.motion.try_send(MotionEvent::Play).expect("room");
    render(&engine, ChannelLayout::STEREO, &[256]);
    let started = transport.segment_generation();
    assert!(started > sought, "a play start is a discontinuity");
    render(&engine, ChannelLayout::STEREO, &[256, 256]);
    assert!(transport.beat() > beat, "rolling");
    assert_eq!(transport.segment_generation(), started, "rolling is not");
}
