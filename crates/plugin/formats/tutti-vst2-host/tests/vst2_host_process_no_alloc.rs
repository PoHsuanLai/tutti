//! RT-safety regression: `Vst2Instance::process_f32` must not allocate
//! on the audio thread in steady state.
//!
//! Mirrors `tutti-clap-host/tests/clap_process_no_alloc.rs` and
//! `tutti-vst3-host/tests/vst3_process_no_alloc.rs`. Requires a real VST2
//! plugin (TAL-NoiseMaker by default) and is `#[ignore]`'d:
//!
//! ```bash
//! cargo test -p tutti-vst2-host --test vst2_host_process_no_alloc -- --ignored
//! ```
//!
//! TAL-NoiseMaker is a stereo synth (0 audio inputs, 2 outputs); the
//! buffer plumbing is hard-coded to that shape to keep all
//! per-iteration setup on the stack.

use std::path::Path;
use std::sync::Mutex;

use assert_no_alloc::AllocDisabler;
use tutti_vst2_host::{
    MidiEvent, ProcessContext, RenderScratch, TimeSignature, TransportInfo, Vst2Instance,
};

// The `assert_no_alloc` checks below are inert unless `AllocDisabler` is the
// active global allocator for THIS test binary. The `#[cfg(test)]` decl in
// `src/lib.rs` does not apply to integration tests, so declare it here —
// matching `tutti-vst3-host` / `tutti-clap-host`.
#[global_allocator]
static A: AllocDisabler = AllocDisabler;

const TAL_NOISEMAKER: &str = "/Library/Audio/Plug-Ins/VST/TAL-NoiseMaker.vst";

static PLUGIN_LOAD_LOCK: Mutex<()> = Mutex::new(());

fn load_or_skip() -> Option<(Vst2Instance, RenderScratch)> {
    let path = Path::new(TAL_NOISEMAKER);
    if !path.exists() {
        eprintln!("TAL-NoiseMaker VST2 not installed at {TAL_NOISEMAKER}, skipping");
        return None;
    }
    // Bundle resolution (.vst → Contents/MacOS/<stem> on macOS) is
    // handled by Vst2Instance::load.
    let inst = Vst2Instance::load(path, 48_000.0, 64).expect("VST2 load failed");
    let meta = inst.metadata().clone();
    let scratch = RenderScratch::new(meta.num_inputs, meta.num_outputs, 64);
    Some((inst, scratch))
}

/// Run the plugin `iters` times with silent input. All buffer setup
/// stays on the stack — no heap allocs per iteration.
fn drive_silent(
    inst: &mut Vst2Instance,
    scratch: &mut RenderScratch,
    iters: usize,
    transport: &TransportInfo,
) {
    let mut out_l = [0.0f32; 64];
    let mut out_r = [0.0f32; 64];
    let ctx = ProcessContext::new(48_000.0).transport(transport);
    for _ in 0..iters {
        let outs: &mut [&mut [f32]] = &mut [&mut out_l[..], &mut out_r[..]];
        let ins: &[&[f32]] = &[];
        let _ = inst.process_f32(ins, outs, 64, &ctx, scratch);
    }
}

#[test]
#[ignore]
fn process_f32_steady_state_does_not_allocate() {
    let _lock = PLUGIN_LOAD_LOCK.lock().unwrap();
    let Some((mut inst, mut scratch)) = load_or_skip() else {
        return;
    };
    let transport = TransportInfo::default()
        .with_playing(true)
        .with_tempo(120.0)
        .with_time_signature(TimeSignature::default());

    // Warm-up: grows the MIDI dispatch buffer + the pooled `midi_out`
    // drain past their inline caps so subsequent calls are heap-free.
    drive_silent(&mut inst, &mut scratch, 32, &transport);

    assert_no_alloc::assert_no_alloc(|| {
        drive_silent(&mut inst, &mut scratch, 256, &transport);
    });
}

#[test]
#[ignore]
fn process_f32_with_midi_does_not_allocate() {
    let _lock = PLUGIN_LOAD_LOCK.lock().unwrap();
    let Some((mut inst, mut scratch)) = load_or_skip() else {
        return;
    };
    let transport = TransportInfo::default()
        .with_playing(true)
        .with_tempo(120.0)
        .with_time_signature(TimeSignature::default());

    let mut out_l = [0.0f32; 64];
    let mut out_r = [0.0f32; 64];

    // Warm-up — note on/off followed by silence so any first-call lazy
    // allocations inside the plugin and the host's pools settle.
    {
        let outs: &mut [&mut [f32]] = &mut [&mut out_l[..], &mut out_r[..]];
        let ins: &[&[f32]] = &[];
        let warm = [
            MidiEvent::note_on(0, 0, 60, 0x4000),
            MidiEvent::note_off(0, 0, 60, 0),
        ];
        let ctx = ProcessContext::new(48_000.0)
            .midi(&warm)
            .transport(&transport);
        let _ = inst.process_f32(ins, outs, 64, &ctx, &mut scratch);
    }
    drive_silent(&mut inst, &mut scratch, 32, &transport);

    let on_event = [MidiEvent::note_on(0, 0, 60, 0x4000)];
    let off_event = [MidiEvent::note_off(0, 0, 60, 0)];

    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..128usize {
            let events: &[MidiEvent] = match i % 32 {
                0 => &on_event,
                16 => &off_event,
                _ => &[],
            };
            let outs: &mut [&mut [f32]] = &mut [&mut out_l[..], &mut out_r[..]];
            let ins: &[&[f32]] = &[];
            let ctx = ProcessContext::new(48_000.0)
                .midi(events)
                .transport(&transport);
            let _ = inst.process_f32(ins, outs, 64, &ctx, &mut scratch);
        }
    });
}
