//! The engine's load meter, end to end: `Engine::process` and
//! `process_segment` time the whole call against the block's budget.
//!
//! The figures come from a node that spins for a known duration, so the
//! load has a floor no scheduler can lower: a spin of `k` budgets is a load
//! of at least `k`. Where a test also needs a ceiling it is generous and
//! says why, because wall time on a shared machine has no upper bound.
//! The meter's arithmetic (mean, peak, reset) is pinned exactly, on fake
//! durations, by the unit tests beside it (`metering/load.rs`).

use assert_no_alloc::AllocDisabler;
use std::time::{Duration, Instant};
use tutti_core::{ChannelLayout, Engine, InterleavedMut, SampleRate, Samples, Transport};
use tutti_graph::{Editor, Prepare, Unforkable};
use tutti_types::graph::{OutPort, Source};
use tutti_types::NodeKey;

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

const RATE: f64 = 48_000.0;

/// A mono source that busy-waits `spin` per graph block, then writes zeros.
struct Spin {
    spin: Duration,
}

impl tutti_graph::Node for Spin {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &tutti_graph::Prepare) {}
    fn process(
        &mut self,
        _: &tutti_graph::Cx<'_>,
        mut io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        let end = Instant::now() + self.spin;
        while Instant::now() < end {
            std::hint::spin_loop();
        }
        io.output(0).fill(0.0);
        tutti_graph::Status::Modified
    }
    fn reset(&mut self) {}
}

/// An engine over one `Spin`, with graph blocks of at most `max_block`.
fn engine(spin: Duration, max_block: usize) -> (Engine, Transport, Editor) {
    let transport = Transport::new(RATE);
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(RATE), Samples(max_block)));
    ed.insert(NodeKey(1), "spin", Unforkable(Spin { spin }));
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort {
        node: NodeKey(1),
        port: 0,
    })];
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");
    (engine, transport, ed)
}

fn budget(frames: usize) -> Duration {
    Duration::from_secs_f64(frames as f64 / RATE)
}

/// A block whose render spins for twice its budget reads a load of at least
/// two, counts one block over budget, and shows as the peak exactly once.
///
/// Mutations (run): `record` never counting over budget → `over_budget` 0 →
/// fails; `Engine::process` not recording → `blocks` 0 → fails; the
/// elapsed time recorded as zero → no block over budget → fails.
#[test]
fn a_block_past_its_deadline_counts_and_peaks_once() {
    let (engine, _t, _ed) = engine(budget(64) * 2, 64);
    let meter = std::sync::Arc::clone(engine.load_meter());
    let mut out = vec![0.0f32; 64];
    engine.process(&mut InterleavedMut::new(&mut out, ChannelLayout::MONO));
    let r = meter.take();
    assert_eq!(r.blocks, 1);
    assert_eq!(r.over_budget, 1);
    assert!(r.last >= 2.0, "{}", r.last);
    assert!(r.peak >= 2.0, "{}", r.peak);
    assert_eq!(meter.take().peak, 0.0, "the take reset the peak");
}

/// The budget is the whole call's frames, summed over the graph blocks a
/// device block splits into: 8 192 frames over a 4 096-frame `MaxBlock` is
/// two graph blocks, each spinning 1.5 budgets of 4 096 frames, so the
/// call's load is at least 1.5, and `process_segment` measures the same way.
///
/// The ceiling (2.9) is the one wall-time bound. The blocks are long so
/// that it is loose in time: the load is over the 8 192-frame call, so it
/// leaves the rest of the call 1.4 × 8 192 frames (about 239 ms) for a
/// graph of one node, which holds on a heavily loaded machine (a
/// 1 024-frame version, with about 30 ms of slack, failed 2 in 200 under
/// load), and it still catches a budget of one piece (load ≥ 3).
///
/// Mutations (run): `budget = ` instead of `budget +=` in `process` → the
/// budget is one piece, load ≥ 3.0 → fails; the same in `process_segment`
/// → fails.
#[test]
fn the_budget_is_the_whole_calls_frames_across_graph_blocks() {
    let (engine, _t, _ed) = engine(budget(4096).mul_f64(1.5), 4096);
    let meter = std::sync::Arc::clone(engine.load_meter());
    let mut out = vec![0.0f32; 8192];
    engine.process(&mut InterleavedMut::new(&mut out, ChannelLayout::MONO));
    let r = meter.take();
    assert!((1.5..2.9).contains(&r.last), "process: {}", r.last);
    assert_eq!(r.over_budget, 1);
    engine.process_segment(&mut InterleavedMut::new(&mut out, ChannelLayout::MONO));
    let r = meter.take();
    assert!((1.5..2.9).contains(&r.last), "segment: {}", r.last);
    assert_eq!((r.blocks, r.over_budget), (2, 2));
}

/// Recording the load is allocation-free: the gate runs `process` and
/// `process_segment` (each recording a block) and the reads.
///
/// Mutation (run): a `Vec` allocated in `LoadMeter::record` → the gate
/// panics → fails.
#[test]
fn recording_the_load_is_allocation_free() {
    let (engine, _t, mut ed) = engine(Duration::ZERO, 64);
    let mut out = vec![0.0f32; 64];
    // Installing the commit allocates on the first block, before the gate.
    engine.process(&mut InterleavedMut::new(&mut out, ChannelLayout::MONO));
    ed.collect();
    let meter = engine.load_meter();
    meter.reset();
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..16 {
            engine.process(&mut InterleavedMut::new(&mut out, ChannelLayout::MONO));
            engine.process_segment(&mut InterleavedMut::new(&mut out, ChannelLayout::MONO));
            let _ = meter.take();
        }
    });
    assert_eq!(meter.take().blocks, 32);
}

/// A mono source that also writes `per_block` events to its one event
/// output each block, at the default capacity.
struct Burst {
    per_block: usize,
}

impl tutti_graph::Node for Burst {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO).with_events(0, 1)
    }
    fn prepare(&mut self, _: &tutti_graph::Prepare) {}
    fn process(
        &mut self,
        _: &tutti_graph::Cx<'_>,
        mut io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        io.output(0).fill(0.0);
        let out = io.event_out(0);
        for _ in 0..self.per_block {
            // A note-on; a full port refuses it, which is the point.
            let _ = out.push(tutti_graph::Event::midi(
                tutti_graph::Offset::ZERO,
                [0x2090_3c7f, 0, 0, 0],
            ));
        }
        tutti_graph::Status::Modified
    }
    fn reset(&mut self) {}
}

/// Events a node writes past its port's capacity reach the load meter: a
/// node writing 100 more than the default capacity each block reads 100
/// dropped per device block, from `process` and `process_segment` both,
/// and a reset zeroes the count.
///
/// Mutations (run): `Engine::process` not forwarding the executor's count
/// (`record_dropped_events` dropped) → 0 after the first block → fails;
/// `take_dropped_events` returning the executor's running total instead
/// of the change → 300 after the second block → fails.
#[test]
fn events_past_a_ports_capacity_reach_the_meter() {
    let transport = Transport::new(RATE);
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(RATE), Samples(64)));
    let per_block = tutti_graph::DEFAULT_EVENT_CAPACITY + 100;
    ed.insert(NodeKey(1), "burst", Unforkable(Burst { per_block }));
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort {
        node: NodeKey(1),
        port: 0,
    })];
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");
    let meter = std::sync::Arc::clone(engine.load_meter());
    let mut out = vec![0.0f32; 64];
    engine.process(&mut InterleavedMut::new(&mut out, ChannelLayout::MONO));
    assert_eq!(meter.peek().dropped_events, 100);
    engine.process(&mut InterleavedMut::new(&mut out, ChannelLayout::MONO));
    assert_eq!(meter.peek().dropped_events, 200);
    engine.process_segment(&mut InterleavedMut::new(&mut out, ChannelLayout::MONO));
    assert_eq!(meter.peek().dropped_events, 300);
    meter.reset();
    assert_eq!(meter.peek().dropped_events, 0);
    engine.process(&mut InterleavedMut::new(&mut out, ChannelLayout::MONO));
    assert_eq!(meter.take().dropped_events, 100);
}
