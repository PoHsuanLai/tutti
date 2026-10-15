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

/// With a project meter installed (`set_meter`), each block reads it — the
/// meter cell's load, then its `RtPublish` — and still does not allocate.
///
/// Mutation (run): allocate in `TransportFeed::block` only when a meter is
/// installed → this test trips the gate, the two without a meter pass.
#[test]
fn process_with_a_meter_does_not_allocate() {
    use std::sync::Arc;
    use tutti_core::{
        Beat, BeatsPerBar, MeterChange, MeterMap, NoteValue, RtPublish, TimeSignature,
    };

    let _lock = PLUGIN_LOAD_LOCK.lock().unwrap();
    let (unit, handle) = tutti_plugin::in_process_vst2_client(probe_path::probe_path(), 48_000.0)
        .expect("load failed");
    unit.set_meter(Arc::new(RtPublish::new(MeterMap::new([MeterChange::new(
        Beat(0.0),
        TimeSignature::new(BeatsPerBar::new(7), NoteValue::EIGHTH),
    )]))));
    let mut unit = Direct::new(unit, SampleRate(48_000.0), 64);
    for _ in 0..32 {
        unit.block();
    }
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..256 {
            unit.block();
        }
    });
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

/// **Two hundred parameters moving in every block do not allocate.** The
/// probe is loaded with 200 parameters, and every block carries two ramps
/// for each of them (frames 0 and 32), every value new: the node's slots,
/// its block's queues and their point room, and the segment slots are all
/// reserved at load and prepare, and the plugin is set through
/// `Vst2Instance::set_parameter`, which reads its bound without allocating.
/// After the gate the plugin holds each parameter's last value, and nothing
/// was dropped or merged for want of room.
///
/// Mutations (run), each fails (aborts inside the gate): the parameter
/// bound read through `get_info()` again in `Vst2Instance::param_index`
/// (it clones the plugin's name and vendor); the node's staged list rebuilt
/// per block (`self.staged = Vec::new()` for `clear()` in
/// `ParamDelivery::settle`).
#[test]
fn two_hundred_moving_parameters_do_not_allocate() {
    use tutti_graph::ParamRamp;

    const PARAMS: u32 = 200;
    let _lock = PLUGIN_LOAD_LOCK.lock().unwrap();
    // SAFETY: the lock serializes the tests here, and nothing else in this
    // process touches the environment. The probe reads it in its constructor.
    unsafe { std::env::set_var("TUTTI_VST2_PROBE_PARAMS", PARAMS.to_string()) };
    let loaded = tutti_plugin::in_process_vst2_client(probe_path::probe_path(), 48_000.0);
    // SAFETY: as above.
    unsafe { std::env::remove_var("TUTTI_VST2_PROBE_PARAMS") };
    let (unit, handle) = loaded.expect("load failed");
    let controls = unit.controls();
    let mut unit = Direct::new(unit, SampleRate(48_000.0), 64);

    // Sixteenths, a different one for every parameter, frame and block.
    let value = |b: u32, k: u32, id: u32| ((b * 2 + k + id) % 16) as f32 / 16.0;
    let mut events: Vec<Event> = Vec::with_capacity(2 * PARAMS as usize);
    let mut block = |unit: &mut Direct<tutti_plugin::InProcessVst2Client>, b: u32| {
        events.clear();
        for k in 0..2 {
            let at = Offset::new(32 * k as usize, Samples(64)).expect("inside");
            for id in 0..PARAMS {
                let ramp = ParamRamp::foreign(id, value(b, k, id), Samples(0));
                events.push(Event::ramp(at, ramp));
            }
        }
        unit.events(0, &events);
        unit.block();
    };
    for b in 0..8 {
        block(&mut unit, b);
    }
    assert_no_alloc::assert_no_alloc(|| {
        for b in 8..72 {
            block(&mut unit, b);
        }
    });

    for id in 0..PARAMS {
        assert_eq!(
            handle
                .params()
                .parameter_value(tutti_plugin::handles::ParamAddress::Index(id as i32)),
            Some(value(71, 1, id)),
            "parameter {id}"
        );
    }
    let stats = controls.param_delivery();
    assert_eq!(stats.points_dropped, 0, "{stats:?}");
    assert_eq!(stats.points_merged, 0, "{stats:?}");
    drop(handle);
}
