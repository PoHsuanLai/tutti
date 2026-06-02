//! RT-safety regression: the in-process VST2 audio path must not
//! allocate on the audio thread in steady state.
//!
//! Mirrors the harness in `tutti-core/tests/rt_no_alloc.rs`. Requires
//! a real plugin (TAL-NoiseMaker) and is `#[ignore]`'d by default —
//! some plugins allocate inside their `process` callback, which would
//! also trip the harness; this test only catches our host-side
//! regressions, so failure means the in-process backend (or the
//! `vst2-host` codec) introduced a per-block alloc.

#![cfg(feature = "vst2-in-process")]

use assert_no_alloc::AllocDisabler;
use std::path::Path;
use std::sync::Mutex;

use tutti_core::BufferVec;
use tutti_midi_types::convert::midi1_velocity_to_midi2;
use tutti_midi_types::ump::MidiEvent;

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

const VST2_PLUGIN: &str = "/Library/Audio/Plug-Ins/VST/TAL-NoiseMaker.vst";

static PLUGIN_LOAD_LOCK: Mutex<()> = Mutex::new(());

#[test]
#[ignore]
fn process_steady_state_does_not_allocate() {
    let _lock = PLUGIN_LOAD_LOCK.lock().unwrap();
    let (mut unit, handle) =
        tutti_plugin::in_process_vst2(Path::new(VST2_PLUGIN), 48_000.0).expect("load failed");

    let meta = handle.metadata();
    let in_ch = meta.audio_io.inputs.max(1); // BufferVec::new(0) panics on at()
    let out_ch = meta.audio_io.outputs.max(1);

    let input = BufferVec::new(in_ch);
    let mut output = BufferVec::new(out_ch);

    // Warm up: many plugins allocate on their first few process calls
    // (sample buffers, lookup tables). Run enough blocks to settle.
    for _ in 0..32 {
        unit.process(64, &input.buffer_ref(), &mut output.buffer_mut());
    }

    // Steady state: no allocation per block.
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..256 {
            unit.process(64, &input.buffer_ref(), &mut output.buffer_mut());
        }
    });

    // Keep the handle alive past the assertion — dropping it would
    // free the Arc and could allocate as part of teardown. (Outside
    // the assert block this is fine.)
    drop(handle);
}

#[test]
#[ignore]
fn process_with_midi_does_not_allocate() {
    let _lock = PLUGIN_LOAD_LOCK.lock().unwrap();
    let (mut unit, handle) =
        tutti_plugin::in_process_vst2(Path::new(VST2_PLUGIN), 48_000.0).expect("load failed");

    let sender = handle.midi_sender();

    let meta = handle.metadata();
    let in_ch = meta.audio_io.inputs.max(1);
    let out_ch = meta.audio_io.outputs.max(1);

    let input = BufferVec::new(in_ch);
    let mut output = BufferVec::new(out_ch);

    // Warm up audio path.
    for _ in 0..32 {
        unit.process(64, &input.buffer_ref(), &mut output.buffer_mut());
    }

    // Pre-warm MIDI codec — first event triggers any one-shot
    // allocations inside vst2-host's MidiSendBuffer.
    let warm_event = MidiEvent::note_on(0, 1, 60, midi1_velocity_to_midi2(100));
    sender.queue(&[warm_event]);
    unit.process(64, &input.buffer_ref(), &mut output.buffer_mut());
    let warm_off = MidiEvent::note_off(0, 1, 60, 0);
    sender.queue(&[warm_off]);
    unit.process(64, &input.buffer_ref(), &mut output.buffer_mut());

    // Steady state with periodic MIDI: no allocation.
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..128 {
            if i % 16 == 0 {
                let ev =
                    MidiEvent::note_on(0, 1, 60 + (i as u8 % 12), midi1_velocity_to_midi2(100));
                sender.queue(&[ev]);
            }
            if i % 16 == 8 {
                let ev = MidiEvent::note_off(0, 1, 60 + (i as u8 % 12), 0);
                sender.queue(&[ev]);
            }
            unit.process(64, &input.buffer_ref(), &mut output.buffer_mut());
        }
    });

    drop(handle);
}
