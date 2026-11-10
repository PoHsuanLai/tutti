//! The loop region is the engine's.
//!
//! An edit is a scheduled `TransportCommand::Loop`; the engine applies it on
//! its frame and publishes a snapshot that any thread reads whole. What is
//! pinned:
//!
//! - every frame a node renders sees exactly the region the schedule says,
//!   never the start of one edit and the end of another;
//! - an edit lands on the frame (or block) it names, and the snapshot follows
//!   the engine, not the call that sent the edit;
//! - disarming keeps the bounds, and arming restores them;
//! - a thread reading the snapshot while the engine applies edits reads one
//!   of the regions written.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tutti_engine::Engine;
use tutti_graph::NodeKey;
use tutti_graph::{Cx, Editor, Io, Node, Prepare, Shape, Status, Unforkable};
use tutti_graph::{OutPort, Source};
use tutti_transport::{LoopEdit, LoopRegion, Transport};
use tutti_types::{At, ChannelLayout, Frame, InterleavedMut, LoopRange, SampleRate, Samples, Tail};

const SR: f64 = 48_000.0;

/// What a node reads of the loop at one frame.
type Seen = Option<(f64, f64)>;

/// Logs, for every frame it renders, the loop its `Env` gives that frame
/// (`transport_at`): the audio-side read. Logs nothing when given no log.
struct LoopProbe(Option<Arc<Mutex<Vec<Seen>>>>);

impl Node for LoopProbe {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO).with_tail(Tail::Unbounded)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let mut log = self.0.as_ref().map(|l| l.lock().expect("log"));
        for k in cx.env.offsets() {
            let looping = cx.env.transport_at(k).looping;
            if let Some(log) = log.as_mut() {
                log.push(looping.map(|l| (l.start.get(), l.end.get())));
            }
        }
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

fn engine(transport: &Transport) -> (Engine, Editor, Arc<Mutex<Vec<Seen>>>) {
    let log = Arc::default();
    let (engine, ed) = engine_logging(transport, Some(Arc::clone(&log)));
    (engine, ed, log)
}

fn engine_logging(transport: &Transport, log: Option<Arc<Mutex<Vec<Seen>>>>) -> (Engine, Editor) {
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(SR), Samples(512)));
    ed.insert(NodeKey(1), "probe", Unforkable(LoopProbe(log)));
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort {
        node: NodeKey(1),
        port: 0,
    })];
    ed.commit().expect("commits");
    let engine = Engine::new(transport, &mut ed, exec).expect("within the limits");
    (engine, ed)
}

fn render(engine: &Engine, frames: usize) {
    let mut buf = vec![0.0f32; frames];
    engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::MONO));
}

fn range(start: f64, end: f64) -> LoopRange {
    LoopRange::new(start, end).expect("ordered")
}

fn pair(r: LoopRange) -> (f64, f64) {
    (r.start().get(), r.end().get())
}

/// Every frame sees the region the schedule says, and only whole regions:
/// edits that move the bounds and edits that arm, alternating at frames that
/// fall inside blocks, with the bounds far enough apart that a start of one
/// beside an end of the other is a different valid range.
///
/// The expectation is computed from the schedule alone: the last edit at or
/// before the frame wins.
///
/// Mutation (run): `Control::apply` ignoring a `Loop` edit (the block's walk
/// keeps the old region) → fails at the first edit.
#[test]
fn every_frame_sees_one_whole_region() {
    let a = range(0.0, 4.0);
    let b = range(8.0, 12.0);
    let transport = Transport::new(SR);
    let (engine, _ed, log) = engine(&transport);

    // (frame, edit): alternating bounds, arming toggled between them.
    let mut schedule = Vec::new();
    for i in 0..24u64 {
        let frame = 90 + i * 211;
        let edit = match i % 4 {
            0 => LoopEdit::over(b),
            1 => LoopEdit::armed(false),
            2 => LoopEdit::bounds(a),
            _ => LoopEdit::armed(true),
        };
        schedule.push((frame, edit));
    }
    for &(frame, edit) in &schedule {
        transport
            .motion
            .schedule(At::Frame(Frame(frame)), edit)
            .expect("room");
    }
    let total = 5_400;
    for _ in 0..total / 450 {
        render(&engine, 450);
    }

    let log = log.lock().expect("log");
    assert_eq!(log.len(), total);
    let mut region = LoopRegion::default();
    let mut seen_a = false;
    let mut seen_b = false;
    for (frame, got) in log.iter().enumerate() {
        for &(at, edit) in &schedule {
            if at == frame as u64 {
                region = region.with(edit);
            }
        }
        assert_eq!(*got, region.active().map(pair), "frame {frame}");
        seen_a |= *got == Some(pair(a));
        seen_b |= *got == Some(pair(b));
    }
    assert!(seen_a && seen_b, "both regions were rendered under");
}

/// An edit lands on the frame it names: the frame before sees the old
/// region, the frame itself the new. `At::NextBlock` lands on the first
/// frame of the next block.
///
/// Mutation (run): `Control::apply` ignoring `Loop` → the named frame still
/// sees the old region → fails. Not covered: a command applied one frame
/// late; the cut machinery is pinned by `engine_graph`'s timed-command tests.
#[test]
fn a_scheduled_edit_lands_on_its_frame() {
    let a = range(0.0, 4.0);
    let b = range(8.0, 12.0);
    let transport = Transport::new(SR);
    let (engine, _ed, log) = engine(&transport);
    let m = &transport.motion;

    m.schedule(At::Frame(Frame(0)), LoopEdit::over(a))
        .expect("room");
    m.schedule(At::Frame(Frame(1_300)), LoopEdit::over(b))
        .expect("room");
    for _ in 0..3 {
        render(&engine, 512); // frames 0..1 536
    }
    // Sent between blocks: lands on the next block's first frame.
    m.schedule(At::NextBlock, LoopEdit::armed(false))
        .expect("room");
    render(&engine, 512); // 1 536..2 048

    let log = log.lock().expect("log");
    assert_eq!(log[1_299], Some(pair(a)), "the frame before");
    assert_eq!(log[1_300], Some(pair(b)), "the named frame");
    assert_eq!(log[1_535], Some(pair(b)), "the last frame of the block");
    assert_eq!(log[1_536], None, "the first frame of the next block");
}

/// Disarming keeps the bounds and arming restores them, on the audio side
/// and in the snapshot; moving the bounds while disarmed does not arm.
///
/// Mutation (run): `LoopOwner::apply` resetting the bounds to the default
/// when disarming → the snapshot's bounds are 0..16 after the disarm → fails.
#[test]
fn disarming_keeps_the_bounds() {
    let a = range(2.0, 6.0);
    let b = range(10.0, 14.0);
    let transport = Transport::new(SR);
    let (engine, _ed, log) = engine(&transport);
    let m = &transport.motion;

    m.schedule(At::NextBlock, LoopEdit::over(a)).expect("room");
    render(&engine, 100);
    assert_eq!(m.loop_region(), LoopRegion::new(a, true));

    m.schedule(At::NextBlock, LoopEdit::armed(false))
        .expect("room");
    render(&engine, 100);
    let region = m.loop_region();
    assert!(!region.is_armed());
    assert_eq!(region.bounds(), a, "the bounds survive disarming");
    assert_eq!(region.active(), None);

    // Moving the bounds while disarmed does not arm.
    m.schedule(At::NextBlock, LoopEdit::bounds(b))
        .expect("room");
    render(&engine, 100);
    assert_eq!(m.loop_region(), LoopRegion::new(b, false));

    m.schedule(At::NextBlock, LoopEdit::armed(true))
        .expect("room");
    render(&engine, 100);
    assert_eq!(m.loop_region(), LoopRegion::new(b, true));

    let log = log.lock().expect("log");
    assert_eq!(log[50], Some(pair(a)));
    assert_eq!(log[150], None);
    assert_eq!(log[250], None, "bounds moved, still disarmed");
    assert_eq!(log[350], Some(pair(b)));
}

/// The snapshot is the engine's: sending an edit does not change it, the
/// block that applies the edit does. An edit due beyond the block waits.
///
/// Mutation (run): drop the publish in `LoopOwner::apply` → the snapshot never
/// follows the engine → fails.
#[test]
fn the_snapshot_follows_the_engine() {
    let a = range(1.0, 3.0);
    let transport = Transport::new(SR);
    let (engine, _ed, _log) = engine(&transport);
    let m = &transport.motion;

    assert_eq!(m.loop_region(), LoopRegion::default(), "before any edit");
    assert_eq!(m.loop_region().bounds(), range(0.0, 16.0));
    assert!(!m.loop_region().is_armed());

    m.schedule(At::Frame(Frame(700)), LoopEdit::over(a))
        .expect("room");
    assert_eq!(m.loop_region(), LoopRegion::default(), "sent, not applied");
    render(&engine, 512);
    assert_eq!(m.loop_region(), LoopRegion::default(), "due past the block");
    render(&engine, 512);
    assert_eq!(m.loop_region(), LoopRegion::new(a, true), "after the block");
}

/// A thread reading the snapshot while the engine applies edits reads a
/// region some edit wrote: bounds A or B, never a start of one and an end of
/// the other (which would be (0, 12), a valid range).
///
/// Probabilistic: it needs the reader to land inside a write, so it runs many
/// edits. Mutation (run): `LoopOwner::apply` first publishing the new start
/// beside the old end, then the whole region → the reader sees mixed pairs
/// → fails.
#[test]
fn a_reader_thread_never_sees_a_torn_region() {
    let a = range(0.0, 4.0);
    let b = range(8.0, 12.0);
    let transport = Transport::new(SR);
    // No log: the probe would append for every frame of this many blocks.
    let (engine, _ed) = engine_logging(&transport, None);
    let done = AtomicBool::new(false);
    let reader_transport = transport.clone();

    std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            let mut reads = 0u64;
            // Reads at least once, whenever the thread starts.
            loop {
                let region = reader_transport.motion.loop_region();
                let bounds = region.bounds();
                assert!(
                    bounds == a || bounds == b || bounds == range(0.0, 16.0),
                    "torn bounds {bounds:?}"
                );
                reads += 1;
                if done.load(Ordering::Acquire) {
                    break;
                }
            }
            reads
        });
        let m = &transport.motion;
        for i in 0..4_000u32 {
            let edit = LoopEdit::over(if i % 2 == 0 { b } else { a });
            while m.schedule(At::NextBlock, edit).is_err() {
                render(&engine, 64);
            }
            render(&engine, 64);
        }
        done.store(true, Ordering::Release);
        assert!(reader.join().expect("reader ran") > 0);
    });
}
