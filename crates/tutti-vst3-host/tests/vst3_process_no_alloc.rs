//! RT-safety regression: `Vst3Instance::process` must not allocate on the
//! audio thread in steady state.
//!
//! Requires a real VST3 plugin (TAL-NoiseMaker by default) and is
//! `#[ignore]`'d. Run with:
//!
//! ```bash
//! cargo test -p tutti-vst3-host --test vst3_process_no_alloc -- --ignored
//! ```
//!
//! Mirrors the pattern in `tutti-plugin/tests/vst2_in_process_no_alloc.rs`
//! and `clap-host/tests/clap_process_no_alloc.rs`. TAL-NoiseMaker is a
//! stereo synth (0 audio inputs, 2 outputs); the buffer plumbing is
//! hard-coded to that shape to keep all per-iteration setup on the stack.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use assert_no_alloc::AllocDisabler;
use tutti_vst3_host::{AudioBuffer, MidiEvent, TransportInfo, Vst3Instance};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

const TAL_NOISEMAKER: &str = "/Library/Audio/Plug-Ins/VST3/TAL-NoiseMaker.vst3";

static PLUGIN_LOAD_LOCK: Mutex<()> = Mutex::new(());

/// Resolve a VST3 bundle directory to its inner binary on macOS.
/// On other platforms / non-bundle layouts, returns the input path
/// unchanged.
fn resolve_bundle(path: &Path) -> PathBuf {
    if path.is_file() || !path.is_dir() {
        return path.to_path_buf();
    }
    #[cfg(target_os = "macos")]
    {
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let candidate = path.join("Contents").join("MacOS").join(stem);
        if candidate.is_file() {
            return candidate;
        }
    }
    path.to_path_buf()
}

fn load_or_skip() -> Option<Vst3Instance> {
    let bundle = Path::new(TAL_NOISEMAKER);
    if !bundle.exists() {
        eprintln!("TAL-NoiseMaker VST3 not installed at {TAL_NOISEMAKER}, skipping");
        return None;
    }
    let library = resolve_bundle(bundle);
    let inst = Vst3Instance::load(&library, 48_000.0, 64).expect("VST3 load failed");
    Some(inst)
}

/// Run the plugin `iters` times with silent input. Buffer setup is done
/// per-iteration but uses only stack arrays — no heap allocs.
fn drive_silent(inst: &mut Vst3Instance, iters: usize, transport: &TransportInfo) {
    let mut out_l = [0.0f32; 64];
    let mut out_r = [0.0f32; 64];
    let midi: [MidiEvent; 0] = [];
    for _ in 0..iters {
        let outs: &mut [&mut [f32]] = &mut [&mut out_l[..], &mut out_r[..]];
        let ins: &[&[f32]] = &[];
        let mut buffer = AudioBuffer::new(ins, outs, 48_000.0);
        let _ = inst.process(&mut buffer, &midi, None, &[], transport);
    }
}

#[test]
#[ignore]
fn process_steady_state_does_not_allocate() {
    let _lock = PLUGIN_LOAD_LOCK.lock().unwrap();
    let Some(mut inst) = load_or_skip() else {
        return;
    };
    let transport = TransportInfo::default().with_tempo(120.0).with_playing(true);

    // Warm up — primes plugin internal state and the host's pooled
    // return buffers (first call grows them to needed capacity).
    drive_silent(&mut inst, 32, &transport);

    assert_no_alloc::assert_no_alloc(|| {
        drive_silent(&mut inst, 256, &transport);
    });
}

#[test]
#[ignore]
fn process_with_midi_does_not_allocate() {
    let _lock = PLUGIN_LOAD_LOCK.lock().unwrap();
    let Some(mut inst) = load_or_skip() else {
        return;
    };
    let transport = TransportInfo::default().with_tempo(120.0).with_playing(true);

    let mut out_l = [0.0f32; 64];
    let mut out_r = [0.0f32; 64];

    // Warm: one note on/off, then 32 silent blocks to flush any
    // first-call lazy allocations inside the plugin or its voice
    // allocator.
    {
        let outs: &mut [&mut [f32]] = &mut [&mut out_l[..], &mut out_r[..]];
        let ins: &[&[f32]] = &[];
        let mut buffer = AudioBuffer::new(ins, outs, 48_000.0);
        let warm = [
            MidiEvent::note_on(0, 0, 60, 0x8000),
            MidiEvent::note_off(0, 0, 60, 0),
        ];
        let _ = inst.process(&mut buffer, &warm, None, &[], &transport);
    }
    drive_silent(&mut inst, 32, &transport);

    let on_event = [MidiEvent::note_on(0, 0, 60, 0x8000)];
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
            let mut buffer = AudioBuffer::new(ins, outs, 48_000.0);
            let _ = inst.process(&mut buffer, events, None, &[], &transport);
        }
    });
}
