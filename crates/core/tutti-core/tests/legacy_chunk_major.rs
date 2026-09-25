//! The graph engine renders chunk-major while its plan holds a `Legacy` unit
//! (doc 013, "chunk-major `Legacy` compatibility mode").
//!
//! A `Legacy` unit reads no `Env`: one that follows the transport polls the
//! live `Transport` on every 64-frame call, and readers of one timeline often
//! share state across nodes — a `BeatCursor` shared by the clones of a voice
//! or a clip source, both halves of a crossfade. `Net` rendered every node
//! 64 frames at a time with its clock moved between chunks, so every such
//! reader saw the timeline only move forward, a chunk at a time. The graph
//! engine does the same: blocks of at most `LEGACY_CHUNK` across all nodes,
//! the playhead published after each. What is pinned here:
//!
//! - a `BeatCursor` shared by two `Legacy` nodes, and by the two halves of a
//!   crossfade, never sees a discontinuity on a rolling transport (a rewind
//!   refires a clip's notes; any discontinuity flushes a voice's vocoder);
//! - the playhead the control thread reads, sampled from another thread
//!   while the engine renders, never goes backwards;
//! - a graph without a `Legacy` unit still renders whole blocks.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tutti_core::transport::{BeatCursor, BeatWindowSync};
use tutti_core::{
    AudioUnit, BufferMut, BufferRef, ChannelLayout, Engine, InterleavedMut, MotionEvent,
    SampleRate, Samples, Signal, SignalFrame, Tail, Timeline, Transport,
};
use tutti_graph::{
    CrossfadeCurve, Cx, Editor, Env, Fade, Io, Legacy, Node, Prepare, Shape, Status, LEGACY_CHUNK,
};
use tutti_types::graph::{OutPort, Source};
use tutti_types::NodeKey;

const SR: f64 = 48_000.0;

/// A clip reader reduced to its clock: an `AudioUnit` holding a
/// [`BeatCursor`] on the live transport, advancing it on every call as a
/// voice pool or a MIDI clip source does, and logging every discontinuity.
/// Clones share the cursor, as a voice's clones do.
#[derive(Clone)]
struct CursorProbe {
    cursor: BeatCursor,
    /// `(call, sync)` for every call that saw the timeline jump.
    jumps: Arc<Mutex<Vec<(usize, BeatWindowSync)>>>,
    calls: Arc<Mutex<usize>>,
}

impl AudioUnit for CursorProbe {
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
        let mut calls = self.calls.lock().expect("calls");
        if let Some((_, sync)) = self.cursor.advance(size) {
            if sync != BeatWindowSync::Rolling {
                self.jumps.lock().expect("jumps").push((*calls, sync));
            }
        }
        *calls += 1;
        for i in 0..size {
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
        tutti_core::mnemonic(b"TCURPROB")
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

/// Logs every block length it is handed: a native node, no `Legacy`.
struct BlockLog(Arc<Mutex<Vec<usize>>>);

impl Node for BlockLog {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO).with_tail(Tail::Unbounded)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let env: &Env = cx.env;
        self.0.lock().expect("log").push(env.block_len.get());
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

fn outputs(ed: &mut Editor, keys: &[u64]) {
    ed.spec_mut().topology.outputs = keys
        .iter()
        .map(|&k| {
            Source::Node(OutPort {
                node: NodeKey(k),
                port: 0,
            })
        })
        .collect();
}

/// **Two `Legacy` clip readers sharing one cursor, and a crossfade of one
/// of them, never see the timeline jump, and the playhead another thread
/// reads never goes backwards**, over two seconds of 512-frame device blocks
/// on a rolling transport.
///
/// The graph also holds a native node that logs its blocks: every one is at
/// most 64 frames, because a `Legacy` unit is present.
///
/// Mutations (run):
/// - the engine rendering whole blocks with a `Legacy` unit present
///   (`GraphRender::settle` ignoring `has_legacy`) → every call of a block
///   reads the beat the last block ended on, the next block's first call is
///   a whole block on, past the cursor's slack → `Jumped` on the first node
///   every block; and the native node logs 512-frame blocks;
/// - the engine publishing its playhead in the walk *and* re-publishing the
///   block's first beat before the render (what a voice should read, set
///   the other way round: `TransportClock::advance` writing back, and
///   `render` storing `transport.beat` before processing) → the reader
///   thread sees it step back to the block's start every block.
#[test]
fn shared_cursors_see_no_jump_and_the_playhead_never_goes_backwards() {
    let transport = Transport::new(SR);
    let jumps = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::new(Mutex::new(0usize));
    let probe = CursorProbe {
        cursor: BeatCursor::new(Arc::new(transport.clone()) as Arc<dyn Timeline>, SR),
        jumps: Arc::clone(&jumps),
        calls: Arc::clone(&calls),
    };
    let blocks = Arc::new(Mutex::new(Vec::new()));

    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(SR), Samples(1024)));
    ed.insert(NodeKey(1), "clip a", Legacy::new(probe.clone()));
    ed.insert(NodeKey(2), "clip b", Legacy::new(probe.clone()));
    ed.insert(NodeKey(3), "blocks", BlockLog(Arc::clone(&blocks)));
    outputs(&mut ed, &[1, 2, 3]);
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");

    // A reader on another thread, as a UI or the mod driver reads the
    // playhead: every value it sees must be at or past the last.
    // The render is far faster than real time (two seconds in tens of
    // milliseconds), so a reader left to the scheduler may not run at all
    // under a loaded test run. Each block therefore waits until the reader
    // has read again: it is live and spinning when the block renders, and
    // `reads` counts at least one per block.
    let stop = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicU64::new(0));
    let reader = {
        let (t, stop, reads) = (transport.clone(), Arc::clone(&stop), Arc::clone(&reads));
        std::thread::spawn(move || {
            let (mut last, mut backwards) = (f64::NEG_INFINITY, Vec::new());
            while !stop.load(Ordering::Acquire) {
                let b = t.beat().get();
                if b < last {
                    backwards.push((last, b));
                }
                last = b;
                reads.fetch_add(1, Ordering::Release);
            }
            (backwards, last)
        })
    };

    transport.motion.try_send(MotionEvent::Play).expect("room");
    let mut buf = vec![0.0f32; 512 * 3];
    let n_blocks = 2 * SR as usize / 512;
    for i in 0..n_blocks {
        if i == n_blocks / 3 {
            // Replace one reader with another clone sharing the cursor, over
            // a 4096-frame fade: both halves poll the timeline meanwhile.
            ed.replace(
                NodeKey(1),
                Legacy::new(probe.clone()),
                Fade::new(Samples(4096), CrossfadeCurve::EqualAmplitude),
            )
            .expect("same shape");
            ed.commit().expect("commits");
        }
        let seen = reads.load(Ordering::Acquire);
        while reads.load(Ordering::Acquire) == seen {
            std::thread::yield_now();
        }
        engine.process(&mut InterleavedMut::new(
            &mut buf,
            ChannelLayout::from_count(3),
        ));
        ed.collect();
    }
    stop.store(true, Ordering::Release);
    let (backwards, last) = reader.join().expect("reader");
    let reads = reads.load(Ordering::Acquire);

    let jumps = jumps.lock().expect("jumps");
    assert!(
        jumps.is_empty(),
        "a shared cursor saw the timeline jump (call, sync): {:?}",
        &jumps[..jumps.len().min(8)]
    );
    assert!(
        backwards.is_empty(),
        "the playhead went backwards {} times, first {:?}",
        backwards.len(),
        backwards.first()
    );
    // Not vacuous: the probes ran every chunk (two nodes, plus the fade's
    // outgoing half), the reader read many times, and time moved two
    // seconds (four beats at 120 BPM).
    let calls = *calls.lock().expect("calls");
    assert!(
        calls >= 2 * n_blocks * 512 / LEGACY_CHUNK,
        "{calls} probe calls"
    );
    assert!(reads > n_blocks as u64, "{reads} reads");
    assert!((last - 4.0).abs() < 0.1, "the playhead ended at {last}");
    let blocks = blocks.lock().expect("blocks");
    assert!(
        blocks.iter().all(|&n| n <= LEGACY_CHUNK),
        "a graph holding a `Legacy` unit renders chunk-major: {:?}",
        &blocks[..blocks.len().min(8)]
    );
}

/// **A graph with no `Legacy` unit renders whole blocks**: chunk-major is
/// the compatibility mode's cost, paid only while one is present. And the
/// switch follows the plan: removing the last `Legacy` unit goes back to
/// whole blocks from the next block.
///
/// Mutation (run): `GraphRender::settle` chunking unconditionally → the
/// native node logs 64-frame blocks and fails.
#[test]
fn a_graph_without_legacy_renders_whole_blocks() {
    let transport = Transport::new(SR);
    let blocks = Arc::new(Mutex::new(Vec::new()));
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(SR), Samples(1024)));
    ed.insert(NodeKey(3), "blocks", BlockLog(Arc::clone(&blocks)));
    let probe = CursorProbe {
        cursor: BeatCursor::new(Arc::new(transport.clone()) as Arc<dyn Timeline>, SR),
        jumps: Arc::default(),
        calls: Arc::default(),
    };
    ed.insert(NodeKey(1), "clip", Legacy::new(probe));
    outputs(&mut ed, &[3, 1]);
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");
    let mut buf = vec![0.0f32; 512 * 2];
    let mut render = || {
        engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
    };
    render();
    assert_eq!(
        std::mem::take(&mut *blocks.lock().expect("log")),
        vec![64; 8],
        "with a `Legacy` unit: chunk-major"
    );

    ed.remove(NodeKey(1));
    outputs(&mut ed, &[3]);
    ed.commit().expect("commits");
    render();
    assert_eq!(
        std::mem::take(&mut *blocks.lock().expect("log")),
        vec![512],
        "without one: the whole device block"
    );
}
