//! RT-safety regression: the in-process VST2 audio path must not
//! allocate on the audio thread in steady state.
//!
//! Mirrors the harness in `tutti-core/tests/rt_no_alloc.rs`, and loads the
//! reference probe (`tutti-vst2-test-plugin`) built from this tree in the same
//! `cargo test` invocation — the same plugin every other VST2 test here uses.
//!
//! Third-party plugins may allocate inside their own `process` callback and
//! trip the harness for a reason that is not our bug; the in-repo probe's
//! `process` we control (the same argument `vst2-host`'s own
//! `vst2_host_process_no_alloc.rs` makes). So it runs by default: a failure
//! means the in-process backend (or the `vst2-host` codec) introduced a
//! per-block alloc.

#![cfg(feature = "vst2")]

use assert_no_alloc::AllocDisabler;
use std::sync::Mutex;

use tutti_core::{SampleRate, Samples};
use tutti_graph::contract::Direct;
use tutti_graph::{Event, Offset};
use tutti_midi_types::convert::midi1_velocity_to_midi2;
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiChannel, MidiGroup};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

#[path = "support/probe_path.rs"]
mod probe_path;

static PLUGIN_LOAD_LOCK: Mutex<()> = Mutex::new(());

/// `event` on frame 0 of a 64-frame block.
fn at_start(event: MidiEvent) -> Event {
    Event::midi(Offset::new(0, Samples(64)).expect("inside"), event.data)
}

#[test]
fn process_steady_state_does_not_allocate() {
    let _lock = PLUGIN_LOAD_LOCK.lock().unwrap();
    let (unit, handle) = tutti_plugin::in_process_vst2_client(probe_path::probe_path(), 48_000.0)
        .expect("load failed");
    // Prepared (outside the gate) as a graph prepares a node, then driven by
    // hand through `Node::process`.
    let mut unit = Direct::new(unit, SampleRate(48_000.0), 64);

    // Warm up: many plugins allocate on their first few process calls
    // (sample buffers, lookup tables). Run enough blocks to settle.
    for _ in 0..32 {
        unit.block();
    }

    // Steady state: no allocation per block.
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..256 {
            unit.block();
        }
    });

    // Keep the handle alive past the assertion — dropping it would
    // free the Arc and could allocate as part of teardown. (Outside
    // the assert block this is fine.)
    drop(handle);
}

#[test]
fn process_with_midi_does_not_allocate() {
    let _lock = PLUGIN_LOAD_LOCK.lock().unwrap();
    let (unit, handle) = tutti_plugin::in_process_vst2_client(probe_path::probe_path(), 48_000.0)
        .expect("load failed");
    let mut unit = Direct::new(unit, SampleRate(48_000.0), 64);

    // Warm up audio path.
    for _ in 0..32 {
        unit.block();
    }

    // Pre-warm MIDI codec — first event triggers any one-shot
    // allocations inside vst2-host's MidiSendBuffer.
    let warm_event = MidiEvent::note_on(
        MidiGroup::FIRST,
        MidiChannel::new(1),
        60,
        midi1_velocity_to_midi2(100),
    );
    unit.events(0, &[at_start(warm_event)]);
    unit.block();
    let warm_off = MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::new(1), 60, 0);
    unit.events(0, &[at_start(warm_off)]);
    unit.block();

    // Steady state with periodic MIDI on the event input: no allocation.
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..128 {
            if i % 16 == 0 {
                let ev = MidiEvent::note_on(
                    MidiGroup::FIRST,
                    MidiChannel::new(1),
                    60 + (i as u8 % 12),
                    midi1_velocity_to_midi2(100),
                );
                unit.events(0, &[at_start(ev)]);
            }
            if i % 16 == 8 {
                let ev = MidiEvent::note_off(
                    MidiGroup::FIRST,
                    MidiChannel::new(1),
                    60 + (i as u8 % 12),
                    0,
                );
                unit.events(0, &[at_start(ev)]);
            }
            unit.block();
        }
    });

    drop(handle);
}
