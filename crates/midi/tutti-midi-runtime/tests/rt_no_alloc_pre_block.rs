//! Regression gate for `MidiPreBlock::run` — the once-per-block MIDI producer
//! the CPAL callback runs before the graph render. Called every buffer whenever
//! MIDI is enabled: poll hardware, route events into unit inboxes, tick the
//! clock — all alloc-free.
//!
//! Covers three cases:
//! 1. No MIDI input source attached (fast-path).
//! 2. Input source with empty `poll_into` return.
//! 3. Input source emitting events that get routed + queued.
//!
//! All three must reuse the pre-allocated event buffer inside `MidiPreBlock`
//! without touching the allocator.

use std::sync::Arc;

use assert_no_alloc::AllocDisabler;
use tutti_midi_runtime::MidiPreBlock;
use tutti_midi_types::tutti_types::{MidiChannel, MidiGroup, RtPublish};
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiIn, MidiRoute, MidiRouter, MidiRoutingSnapshot, MidiUnitId};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// A MIDI input source that copies a fixed pre-built set of events into the
/// caller buffer every call — zero allocation during `poll_into`. Mirrors the
/// hardware `MidiIn`: ignores the unit id, returns everything pending.
struct FixedInput {
    events: Vec<MidiEvent>,
}

impl MidiIn for FixedInput {
    fn poll_block(&self, _block_size: usize, buffer: &mut [MidiEvent]) -> usize {
        let n = self.events.len().min(buffer.len());
        buffer[..n].copy_from_slice(&self.events[..n]);
        n
    }
}

/// Minimal MIDI queue: counts events so we can sanity-check routing ran.
/// `queue` must be alloc-free — a relaxed counter bump is.
struct CountingQueue {
    count: std::sync::atomic::AtomicUsize,
}

impl MidiRouter for CountingQueue {
    fn queue(&self, _unit_id: MidiUnitId, events: &[MidiEvent]) -> usize {
        self.count
            .fetch_add(events.len(), std::sync::atomic::Ordering::Relaxed);
        events.len()
    }
}

#[test]
fn pre_block_run_no_input_is_allocation_free() {
    let routing = Arc::new(RtPublish::from_arc(Arc::new(MidiRoutingSnapshot::empty())));
    let pre = MidiPreBlock::new(routing);

    // Warm up.
    pre.run(512);

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            pre.run(512);
        }
    });
}

#[test]
fn pre_block_run_empty_events_is_allocation_free() {
    let routing = Arc::new(RtPublish::from_arc(Arc::new(MidiRoutingSnapshot::empty())));
    let mut pre = MidiPreBlock::new(routing);
    pre.set_input(Arc::new(FixedInput { events: Vec::new() }));

    pre.run(512);

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            pre.run(512);
        }
    });
}

#[test]
fn pre_block_run_with_routed_events_is_allocation_free() {
    // Real routing snapshot: one route on channel 0 to target unit 42.
    let target = MidiUnitId::new(42);
    let route = MidiRoute::for_channel(0).with_target(target);
    let snapshot = MidiRoutingSnapshot::from_routes(vec![route], None);
    let routing = Arc::new(RtPublish::from_arc(Arc::new(snapshot)));

    // Input: two note events at different frame offsets. `MidiPreBlock` delivers
    // both (each keeps its `frame_offset`), which must be alloc-free.
    let events = vec![
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000).with_frame_offset(0),
        MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0).with_frame_offset(128),
    ];

    let mut pre = MidiPreBlock::new(routing);
    pre.set_input(Arc::new(FixedInput { events }));
    let queue = Arc::new(CountingQueue {
        count: std::sync::atomic::AtomicUsize::new(0),
    });
    pre.set_queue(queue.clone());

    // Warm up — first call primes the AudioThreadCells inside MidiPreBlock.
    pre.run(512);

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            pre.run(512);
        }
    });

    // Routing actually ran (2 events × 1001 calls).
    assert_eq!(
        queue.count.load(std::sync::atomic::Ordering::Relaxed),
        2 * 1_001,
        "each call routes both events to the one target unit"
    );
}
