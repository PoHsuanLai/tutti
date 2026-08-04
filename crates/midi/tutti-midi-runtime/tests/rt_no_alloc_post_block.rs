//! Regression gate for `MidiPostBlock::run` — the once-per-block MIDI consumer
//! the CPAL callback runs *after* the graph render. Called every buffer whenever
//! MIDI is enabled: drain the sink, fan each event out through routing — all
//! alloc-free.
//!
//! The sink half matters as much as the drain: nodes `push` into it from inside
//! `process`, which is the hottest position in the callback. A `push` that
//! allocated would be an allocation per emitted event per block.
//!
//! Covers:
//! 1. Nothing emitted (fast path — the common case for a graph with no MIDI
//!    -emitting plugins).
//! 2. Events emitted and routed to a destination.
//! 3. Events emitted with no route — still drained, still alloc-free.
//! 4. Pushing past capacity, where the overflow is dropped rather than spilled
//!    to the heap.

use std::sync::Arc;

use assert_no_alloc::AllocDisabler;
use tutti_midi_runtime::{MidiOutSink, MidiPostBlock};
use tutti_midi_types::tutti_types::{MidiChannel, MidiGroup};
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiRoute, MidiRouter, MidiRoutingTable, MidiUnitId};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// Counts deliveries without allocating (no `Vec` growth inside the gate).
#[derive(Default)]
struct CountingRouter {
    count: std::sync::atomic::AtomicUsize,
}

impl MidiRouter for CountingRouter {
    fn queue(&self, _unit_id: MidiUnitId, events: &[MidiEvent]) {
        self.count
            .fetch_add(events.len(), std::sync::atomic::Ordering::Relaxed);
    }
}

fn note() -> MidiEvent {
    MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000)
}

/// Build a phase routing everything to one unit.
fn routed() -> (MidiPostBlock, Arc<CountingRouter>) {
    let dest = MidiUnitId::new(21);
    let mut table = MidiRoutingTable::new();
    table.set_routes(vec![MidiRoute::new().with_target(dest)], Some(dest));
    table.commit();

    let router = Arc::new(CountingRouter::default());
    let mut post = MidiPostBlock::new(table.snapshot_arc());
    post.set_queue(router.clone());
    (post, router)
}

#[test]
fn post_block_run_with_empty_sink_is_allocation_free() {
    let (post, _router) = routed();

    // Warm up outside the gate — the first run primes any lazy state.
    post.run();

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            post.run();
        }
    });
}

#[test]
fn push_then_run_with_routes_is_allocation_free() {
    let (post, router) = routed();
    let sink = post.sink();

    // Warm up: the RtEventBuf's inline storage is allocated at construction,
    // and the first drain primes it.
    assert!(sink.push(note()));
    post.run();

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            // Emitting is on the hot path too — a node pushes from `process`.
            for _ in 0..8 {
                let _ = sink.push(note());
            }
            post.run();
        }
    });

    assert!(
        router.count.load(std::sync::atomic::Ordering::Relaxed) > 0,
        "the gate must have exercised real delivery, not an empty fast path"
    );
}

#[test]
fn extend_then_run_is_allocation_free() {
    let (post, _router) = routed();
    let sink = post.sink();
    let batch = [note(), note(), note(), note()];

    let _ = sink.extend(&batch);
    post.run();

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            let _ = sink.extend(&batch);
            post.run();
        }
    });
}

#[test]
fn draining_an_unrouted_sink_is_allocation_free() {
    // No routes at all: every event resolves to no target and is consumed.
    let table = MidiRoutingTable::new();
    let router = Arc::new(CountingRouter::default());
    let mut post = MidiPostBlock::new(table.snapshot_arc());
    post.set_queue(router.clone());
    let sink = post.sink();

    assert!(sink.push(note()));
    post.run();

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            for _ in 0..8 {
                let _ = sink.push(note());
            }
            post.run();
        }
    });

    assert_eq!(
        router.count.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "nothing is routable, so nothing should have been delivered"
    );
}

/// Overflow must **drop**, never spill to the heap — that is the whole reason
/// the sink is a capped buffer rather than a `Vec`.
#[test]
fn overflowing_the_sink_is_allocation_free() {
    let sink = MidiOutSink::new();

    // Warm up past capacity once outside the gate.
    for _ in 0..2_000 {
        let _ = sink.push(note());
    }
    assert!(sink.overflowed(), "precondition: capacity was exceeded");
    sink.clear();

    assert_no_alloc::assert_no_alloc(|| {
        // Deliberately push far past capacity: the excess is dropped in place.
        for _ in 0..4_000 {
            let _ = sink.push(note());
        }
    });

    assert!(sink.overflowed(), "the drop must still be reported");
}
