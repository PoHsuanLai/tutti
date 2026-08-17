//! Regression gate for the **hardware** MIDI poll on the audio thread.
//!
//! `tutti-midi-runtime`'s own `rt_no_alloc_pre_block` gate drives `MidiPreBlock`
//! through a `FixedInput` fake, which copies from a pre-built `Vec` and can
//! never allocate — so it proves nothing about the production input. That is
//! `HardwareMidiInputs`, and reaching it needs this crate, because
//! `tutti-midi-runtime` cannot depend on `tutti-midi-hardware` (the dependency runs
//! the other way).
//!
//! What the real path does that the fake does not: drain N port rings into a
//! timestamp scratch, convert arrival `Instant`s to per-block `frame_offset`s,
//! and copy the result into a fixed-capacity scratch buffer. Each of those is a
//! place a buffer could grow, and growing means `realloc` inside the callback.

use std::sync::Arc;
use std::time::Instant;

use assert_no_alloc::AllocDisabler;
use tutti_midi_hardware::HardwareMidiInputs;
use tutti_midi_runtime::MidiPreBlock;
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiChannel, MidiGroup, RtPublish};
use tutti_midi_types::{MidiRoute, MidiRouter, MidiRoutingSnapshot, MidiUnitId};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// Counts routed events so a test can confirm the poll actually carried
/// something. A relaxed counter bump is alloc-free.
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

/// One route on channel 0 to a target unit, so polled events have somewhere to go.
fn routing() -> Arc<RtPublish<MidiRoutingSnapshot>> {
    let route = MidiRoute::for_channel(0).with_target(MidiUnitId::new(42));
    Arc::new(RtPublish::from_arc(Arc::new(
        MidiRoutingSnapshot::from_routes(vec![route], None),
    )))
}

fn note() -> MidiEvent {
    MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000)
}

/// Feed `count` events into `port` on `inputs`, as a driver callback thread would.
fn feed(inputs: &HardwareMidiInputs, port: usize, count: usize) {
    for _ in 0..count {
        inputs.push_input_event(port, note());
    }
}

/// The steady state: events arriving every block, well under the cap.
#[test]
fn hardware_poll_is_allocation_free() {
    let inputs = Arc::new(HardwareMidiInputs::new(256));
    inputs.set_sample_rate(48_000.0);
    let port = inputs.create_input_port("Test Input");

    let mut pre = MidiPreBlock::new(routing());
    pre.set_input(inputs.clone());
    let queue = Arc::new(CountingQueue {
        count: std::sync::atomic::AtomicUsize::new(0),
    });
    pre.set_queue(Arc::clone(&queue) as Arc<dyn MidiRouter>);

    // Warm up: the first poll primes the scratch buffers, which is the engine's
    // cost rather than the audio thread's.
    feed(&inputs, port, 16);
    pre.run(512);

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..500 {
            feed(&inputs, port, 16);
            pre.run(512);
        }
    });

    assert!(
        queue.count.load(std::sync::atomic::Ordering::Relaxed) > 0,
        "the poll must have carried events — an empty drain would prove nothing"
    );
}

/// The overflow case: more events queued than one block may carry.
///
/// This is the path that can grow both scratch buffers. Four ports of 256
/// events offer 1024 against a 256 cap, so the drain must stop at the cap and
/// leave the rest — without reallocating to hold them.
#[test]
fn hardware_poll_over_capacity_is_allocation_free() {
    let inputs = Arc::new(HardwareMidiInputs::new(256));
    inputs.set_sample_rate(48_000.0);
    let ports: Vec<usize> = (0..4)
        .map(|i| inputs.create_input_port(format!("Input {i}")))
        .collect();

    let mut pre = MidiPreBlock::new(routing());
    pre.set_input(inputs.clone());

    // Warm with a *light* load — one event per port.
    //
    // Warming with the flood would defeat the test: the scratch buffers would
    // grow to the flood's size outside the gate and have nothing left to grow
    // for inside it, so an unbounded drain would pass. The buffers must enter
    // the gate holding only their reserved capacity, exactly as they do on a
    // freshly started stream that then meets a burst.
    for &port in &ports {
        feed(&inputs, port, 1);
    }
    pre.run(512);

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..200 {
            // 4 ports x 256 events against a 256-event cap: the drain must stop
            // at the cap and leave the backlog, rather than growing to hold it.
            for &port in &ports {
                feed(&inputs, port, 256);
            }
            pre.run(512);
        }
    });
}

/// A block size sweep, since `frame_offset` conversion is driven by `nframes`.
#[test]
fn hardware_poll_across_block_sizes_is_allocation_free() {
    let inputs = Arc::new(HardwareMidiInputs::new(256));
    inputs.set_sample_rate(48_000.0);
    let port = inputs.create_input_port("Test Input");

    let mut pre = MidiPreBlock::new(routing());
    pre.set_input(inputs.clone());

    const SIZES: [usize; 5] = [64, 128, 256, 512, 1024];
    for frames in SIZES {
        feed(&inputs, port, 8);
        pre.run(frames);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..100 {
            for frames in SIZES {
                feed(&inputs, port, 8);
                pre.run(frames);
            }
        }
    });
}

/// Events whose arrival predates the block must not allocate on the clamp path.
///
/// `read_inputs` converts an arrival `Instant` into a `frame_offset` and clamps
/// anything that lands past the block. A stale timestamp drives that branch,
/// which the steady-state test above never reaches.
#[test]
fn hardware_poll_with_stale_timestamps_is_allocation_free() {
    let inputs = Arc::new(HardwareMidiInputs::new(256));
    inputs.set_sample_rate(48_000.0);
    let port = inputs.create_input_port("Test Input");

    let mut pre = MidiPreBlock::new(routing());
    pre.set_input(inputs.clone());

    let handle = inputs.get_input_producer_handle(port).expect("port exists");
    // An arrival a full second ago converts to a `samples_ago` far past any
    // block, so every event takes the clamp branch.
    let stale = Instant::now() - std::time::Duration::from_secs(1);

    for _ in 0..16 {
        handle.push(note(), stale);
    }
    pre.run(512);

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..200 {
            for _ in 0..16 {
                handle.push(note(), stale);
            }
            pre.run(512);
        }
    });
}
