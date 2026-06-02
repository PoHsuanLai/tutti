//! RT-safety regression: `ClapInstance::process` must not allocate on the
//! audio thread in steady state.
//!
//! Requires a real CLAP plugin (TAL-NoiseMaker by default) and is
//! `#[ignore]`'d. Run with:
//!
//! ```bash
//! cargo test -p tutti-clap-host --test clap_process_no_alloc -- --ignored
//! ```
//!
//! Mirrors the pattern in `tutti-plugin/tests/vst2_in_process_no_alloc.rs`.
//! A failure means the host introduced a per-block allocation — some
//! plugins also allocate inside their own `process` callback, which would
//! trip the harness too, so failure here is a *signal*, not a definitive
//! host-side bug.
//!
//! TAL-NoiseMaker is a stereo synth (0 audio inputs, 2 outputs); the
//! buffer plumbing below is hard-coded to that shape to keep all
//! per-iteration buffer setup on the stack — no Vec collects inside
//! the no-alloc gate.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use assert_no_alloc::AllocDisabler;
use tutti_clap_host::{AudioBuffer32, ClapInstance, MidiEvent, ProcessContext, TransportInfo};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

const TAL_NOISEMAKER: &str = "/Library/Audio/Plug-Ins/CLAP/TAL-NoiseMaker.clap";

static PLUGIN_LOAD_LOCK: Mutex<()> = Mutex::new(());

/// Resolve a CLAP bundle directory to its inner binary on macOS.
/// On other platforms / non-bundle layouts, returns the input path
/// unchanged; the caller surfaces any load failure.
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

fn load_or_skip() -> Option<ClapInstance> {
    let bundle = Path::new(TAL_NOISEMAKER);
    if !bundle.exists() {
        eprintln!("TAL-NoiseMaker CLAP not installed at {TAL_NOISEMAKER}, skipping");
        return None;
    }
    let library = resolve_bundle(bundle);
    let mut inst = ClapInstance::load_with_library(bundle, Some(&library), 48_000.0, 512)
        .expect("CLAP load failed");
    inst.activate().expect("activate failed");
    Some(inst)
}

/// Run the plugin `iters` times. Buffer setup is done per-iteration but
/// uses only stack arrays — no heap allocs.
fn drive_silent(inst: &mut ClapInstance, iters: usize, transport: &TransportInfo) {
    // Stack-only channel storage. Hard-coded stereo out, zero in.
    let mut out_l = [0.0f32; 64];
    let mut out_r = [0.0f32; 64];
    let ctx = ProcessContext {
        transport: Some(transport),
        ..Default::default()
    };
    for _ in 0..iters {
        // Re-borrow each iteration — the AudioBuffer holds mut slices
        // that don't outlive the call. No heap touch: just two slice
        // reborrows.
        let outs: &mut [&mut [f32]] = &mut [&mut out_l[..], &mut out_r[..]];
        let ins: &[&[f32]] = &[];
        let mut buffer = AudioBuffer32 {
            inputs: ins,
            outputs: outs,
            num_samples: 64,
            sample_rate: 48_000.0,
        };
        let _ = inst.process(&mut buffer, &ctx).expect("process");
    }
}

#[test]
#[ignore]
fn process_steady_state_does_not_allocate() {
    let _lock = PLUGIN_LOAD_LOCK.lock().unwrap();
    let Some(mut inst) = load_or_skip() else {
        return;
    };
    let transport = TransportInfo::default();

    // Warm up to prime the plugin and flush our pool reserves.
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
    let transport = TransportInfo::default();

    let mut out_l = [0.0f32; 64];
    let mut out_r = [0.0f32; 64];

    // Prime: one note-on, one note-off, then 32 silent blocks so any
    // first-call lazy allocations inside the plugin are flushed.
    {
        let outs: &mut [&mut [f32]] = &mut [&mut out_l[..], &mut out_r[..]];
        let ins: &[&[f32]] = &[];
        let mut buffer = AudioBuffer32 {
            inputs: ins,
            outputs: outs,
            num_samples: 64,
            sample_rate: 48_000.0,
        };
        let warm = [
            MidiEvent::note_on(0, 0, 60, 0x8000),
            MidiEvent::note_off(0, 0, 60, 0),
        ];
        let ctx = ProcessContext {
            midi: &warm,
            transport: Some(&transport),
            ..Default::default()
        };
        let _ = inst.process(&mut buffer, &ctx).expect("warm process");
    }
    drive_silent(&mut inst, 32, &transport);

    // Steady-state: send a note every 32 blocks, off 16 blocks later.
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
            let mut buffer = AudioBuffer32 {
                inputs: ins,
                outputs: outs,
                num_samples: 64,
                sample_rate: 48_000.0,
            };
            let ctx = ProcessContext {
                midi: events,
                transport: Some(&transport),
                ..Default::default()
            };
            let _ = inst.process(&mut buffer, &ctx).expect("process");
        }
    });
}
