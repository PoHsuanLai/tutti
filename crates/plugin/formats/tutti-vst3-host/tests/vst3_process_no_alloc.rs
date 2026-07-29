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
use tutti_vst3_host::{AudioBuffer, MidiEvent, TransportInfo, Vst3InputEvents, Vst3Instance};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

const TAL_NOISEMAKER: &str = "/Library/Audio/Plug-Ins/VST3/TAL-NoiseMaker.vst3";

static PLUGIN_LOAD_LOCK: Mutex<()> = Mutex::new(());

/// Resolve a VST3 bundle directory to its inner binary, across platforms.
/// A path that is already a file (or not a bundle) is returned unchanged.
fn resolve_bundle(path: &Path) -> PathBuf {
    if path.is_file() || !path.is_dir() {
        return path.to_path_buf();
    }
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    for (sub, ext) in [
        ("Contents/MacOS", ""),
        ("Contents/x86_64-linux", "so"),
        ("Contents/x86_64-win", "vst3"),
    ] {
        let dir = path.join(sub);
        let candidate = if ext.is_empty() {
            dir.join(stem)
        } else {
            dir.join(format!("{stem}.{ext}"))
        };
        if candidate.is_file() {
            return candidate;
        }
    }
    path.to_path_buf()
}

/// Find a plugin to drive: any `.vst3` under `VST3_SAMPLE_PLUGIN_DIR` (the
/// SDK sample plugins the conformance harness builds), else TAL-NoiseMaker at
/// its stock macOS location.
///
/// The env-var path matters on Linux/CI, where no plugin is installed system
/// wide — without it these tests silently skipped on every platform but a
/// developer Mac with that one plugin installed.
fn find_plugin() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("VST3_SAMPLE_PLUGIN_DIR") {
        let dir = PathBuf::from(dir);
        if let Ok(entries) = std::fs::read_dir(&dir) {
            let mut candidates: Vec<PathBuf> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "vst3"))
                .map(|p| resolve_bundle(&p))
                .filter(|p| p.is_file())
                .collect();
            candidates.sort();
            if let Some(first) = candidates.into_iter().next() {
                return Some(first);
            }
        }
    }
    let bundle = Path::new(TAL_NOISEMAKER);
    bundle.exists().then(|| resolve_bundle(bundle))
}

fn load_or_skip() -> Option<Vst3Instance> {
    let Some(library) = find_plugin() else {
        eprintln!(
            "no VST3 plugin found (set VST3_SAMPLE_PLUGIN_DIR, or install \
             TAL-NoiseMaker at {TAL_NOISEMAKER}); skipping"
        );
        return None;
    };
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
        let events = Vst3InputEvents {
            midi: &midi,
            ..Default::default()
        };
        let _ = inst.process(&mut buffer, &events, None, transport);
    }
}

#[test]
#[ignore]
fn process_steady_state_does_not_allocate() {
    let _lock = PLUGIN_LOAD_LOCK.lock().unwrap();
    let Some(mut inst) = load_or_skip() else {
        return;
    };
    let transport = TransportInfo::default()
        .with_tempo(120.0)
        .with_playing(true);

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
    let transport = TransportInfo::default()
        .with_tempo(120.0)
        .with_playing(true);

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
        let events = Vst3InputEvents {
            midi: &warm,
            ..Default::default()
        };
        let _ = inst.process(&mut buffer, &events, None, &transport);
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
            let src = Vst3InputEvents {
                midi: events,
                ..Default::default()
            };
            let _ = inst.process(&mut buffer, &src, None, &transport);
        }
    });
}
