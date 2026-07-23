//! Regression gate for `MidiProcessor::process` — the MIDI-aware
//! AudioProcessor decorator around `GraphProcessor`. Called from the
//! CPAL callback every buffer whenever MIDI is enabled.
//!
//! Covers three cases:
//!
//! 1. No MIDI input source attached (fast-path).
//! 2. Input source with empty `cycle_read` return.
//! 3. Input source emitting events that get routed + queued.
//!
//! All three must reuse the pre-allocated `event_buffer` inside
//! `MidiProcessor` without touching the allocator.

#![cfg(all(feature = "std", feature = "midi"))]

use std::sync::Arc;

use arc_swap::ArcSwap;
use assert_no_alloc::AllocDisabler;
use parking_lot::Mutex;
use tutti_core::processor::{AudioProcessor, GraphProcessor, MidiProcessor};
use tutti_core::{GraphNet, Transport, TransportClock};
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiInputSource, MidiQueue, MidiRoute, MidiRoutingSnapshot, MidiUnitId};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// Build a GraphProcessor fronted by a minimal GraphNet, mirroring the
/// pattern in `tutti/src/audio_io.rs::tests::build_callback_state`.
fn build_graph_processor() -> GraphProcessor {
    let sample_rate = 48_000.0;
    let transport = Transport::new(sample_rate);

    let mut net = GraphNet::new(0, 2);
    let clock = TransportClock::from_inputs(transport.clock_inputs(), sample_rate)
        .with_position_writeback(Arc::clone(&transport.settings.beat));
    net.inner_mut().push(Box::new(clock));
    let backend = net.backend();

    // Keep the net alive for the test's lifetime — the backend borrows it.
    let _leaked: &'static Mutex<GraphNet> = Box::leak(Box::new(Mutex::new(net)));

    GraphProcessor::new(transport.motion.clone(), backend)
}

// A MIDI input source that returns a fixed pre-built slice every call —
// zero allocation during cycle_read.
struct FixedInput {
    events: Vec<(usize, MidiEvent)>,
}

impl MidiInputSource for FixedInput {
    fn cycle_read(&self, _nframes: usize) -> &[(usize, MidiEvent)] {
        &self.events
    }
}

// Minimal MIDI queue: counts calls so we can sanity-check routing ran.
// `queue` must be alloc-free — a relaxed counter bump is.
struct CountingQueue {
    count: std::sync::atomic::AtomicUsize,
}

impl MidiQueue for CountingQueue {
    fn queue(&self, _unit_id: MidiUnitId, events: &[MidiEvent]) {
        self.count
            .fetch_add(events.len(), std::sync::atomic::Ordering::Relaxed);
    }
}

#[test]
fn midi_processor_process_no_input_is_allocation_free() {
    let routing = Arc::new(ArcSwap::new(Arc::new(MidiRoutingSnapshot::empty())));
    let mp = MidiProcessor::new(build_graph_processor(), routing);

    let mut output = vec![0.0f32; 512 * 2];

    // Warm up.
    mp.process(&mut output, 512);

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            mp.process(&mut output, 512);
        }
    });
}

#[test]
fn midi_processor_process_empty_events_is_allocation_free() {
    let routing = Arc::new(ArcSwap::new(Arc::new(MidiRoutingSnapshot::empty())));
    let mut mp = MidiProcessor::new(build_graph_processor(), routing);
    mp.set_input(Arc::new(FixedInput { events: Vec::new() }));

    let mut output = vec![0.0f32; 512 * 2];
    mp.process(&mut output, 512);

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            mp.process(&mut output, 512);
        }
    });
}

#[test]
fn midi_processor_process_with_routed_events_is_allocation_free() {
    // Real routing snapshot: one route on channel 0 to target unit 42.
    let target = MidiUnitId::new(42);
    let route = MidiRoute::for_channel(0).with_target(target);
    let snapshot = MidiRoutingSnapshot::from_routes(vec![route], None);
    let routing = Arc::new(ArcSwap::new(Arc::new(snapshot)));

    // Input: two note events at different frame offsets (forces the
    // sub-buffer split path).
    let events = vec![
        (
            0usize,
            MidiEvent::note_on(0, 0, 60, 0x8000).with_frame_offset(0),
        ),
        (
            0usize,
            MidiEvent::note_off(0, 0, 60, 0).with_frame_offset(128),
        ),
    ];

    let mut mp = MidiProcessor::new(build_graph_processor(), routing);
    mp.set_input(Arc::new(FixedInput { events }));
    mp.set_queue(Arc::new(CountingQueue {
        count: std::sync::atomic::AtomicUsize::new(0),
    }));

    let mut output = vec![0.0f32; 512 * 2];

    // Warm up — first call primes the AudioThreadCells inside MidiProcessor.
    mp.process(&mut output, 512);

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            mp.process(&mut output, 512);
        }
    });
}
