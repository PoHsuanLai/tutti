//! RT-safety regression: `Vst3Active::process` must not allocate on the
//! audio thread in steady state.
//!
//! Requires a real VST3 plugin. `#[ignore]`'d because the global allocator is
//! swapped for `AllocDisabler` binary-wide: an allocation anywhere in the
//! guarded region **aborts the process**, so a skipped plugin or an unrelated
//! panic takes the whole test binary with it rather than failing one case.
//! Run with:
//!
//! ```bash
//! VST3_SAMPLE_PLUGIN_DIR=/path/to/build/VST3/Release \
//! cargo test -p tutti-vst3-host --test vst3_process_no_alloc -- --ignored
//! ```
//!
//! Mirrors the pattern in `tutti-plugin/tests/vst2_in_process_no_alloc.rs`
//! and `clap-host/tests/clap_process_no_alloc.rs`.
//!
//! ## What each test is actually guarding
//!
//! The buffer plumbing is a fixed stereo pair so all per-iteration setup stays
//! on the stack; only the *host* is under test, never the harness.
//!
//! Note that the silent and MIDI cases pass `None` for parameter changes, so
//! neither reaches the automation path — they were measured passing with a
//! deliberate `vec![]` inserted into `ParamValueQueueImpl::refill_from_queue`.
//! [`process_with_automation_does_not_allocate`] is what covers that path, and
//! it catches the same mutation. Any future RT assertion here should be
//! mutation-tested the same way before it is trusted.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use assert_no_alloc::AllocDisabler;
use tutti_midi_types::{MidiChannel, MidiGroup};
use tutti_plugin_types::ParamAddress;
use tutti_vst3_host::{
    AudioBuffer, MidiEvent, ParameterChanges, TransportInfo, Vst3Active, Vst3InputEvents,
};

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

fn load_or_skip() -> Option<Vst3Active> {
    let Some(library) = find_plugin() else {
        eprintln!(
            "no VST3 plugin found (set VST3_SAMPLE_PLUGIN_DIR, or install \
             TAL-NoiseMaker at {TAL_NOISEMAKER}); skipping"
        );
        return None;
    };
    let inst = Vst3Active::load(&library, 48_000.0, 64).expect("VST3 load failed");
    Some(inst)
}

/// Run the plugin `iters` times with silent input. Buffer setup is done
/// per-iteration but uses only stack arrays — no heap allocs.
fn drive_silent(inst: &mut Vst3Active, iters: usize, transport: &TransportInfo) {
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

/// Two parameters, several points each, deliberately **not** in ascending
/// sample-offset order.
///
/// The unsorted order is the point. `ParamValueQueueImpl::refill_from_queue`
/// sorts on the audio thread (VST3 requires ascending offsets, and callers
/// assembling automation from several sources have no obligation to interleave
/// them in order). A sort that spills — or an `is_sorted` fast path that stops
/// covering the sort behind it — allocates in the callback, and only an
/// unsorted input reaches that branch at all.
///
/// Point count stays under `INLINE_POINTS` (16) per queue so a correct
/// implementation has nothing to grow into.
fn unsorted_automation() -> ParameterChanges {
    let mut changes = ParameterChanges::new();
    let lanes: [(u32, &[i32]); 2] = [(100, &[48, 0, 32, 16, 8]), (101, &[56, 24, 60, 4])];
    for (id, offsets) in lanes {
        for (i, off) in offsets.iter().enumerate() {
            changes.add_change(ParamAddress::Opaque(id.into()), *off, i as f64 / 8.0);
        }
    }
    changes
}

#[test]
#[ignore]
fn process_with_automation_does_not_allocate() {
    let _lock = PLUGIN_LOAD_LOCK.lock().unwrap();
    let Some(mut inst) = load_or_skip() else {
        return;
    };
    let transport = TransportInfo::default()
        .with_tempo(120.0)
        .with_playing(true);

    // Built once, off the audio thread: the assertion is about what the host
    // does per block, not about assembling the automation.
    let changes = unsorted_automation();

    let mut out_l = [0.0f32; 64];
    let mut out_r = [0.0f32; 64];
    let midi: [MidiEvent; 0] = [];

    let mut drive = |inst: &mut Vst3Active, iters: usize| {
        for _ in 0..iters {
            let outs: &mut [&mut [f32]] = &mut [&mut out_l[..], &mut out_r[..]];
            let ins: &[&[f32]] = &[];
            let mut buffer = AudioBuffer::new(ins, outs, 48_000.0);
            let events = Vst3InputEvents {
                midi: &midi,
                ..Default::default()
            };
            let _ = inst.process(&mut buffer, &events, Some(&changes), &transport);
        }
    };

    // Warm: the first block grows the host's pooled queue storage to the
    // needed capacity. Steady state must then reuse it.
    drive(&mut inst, 32);

    assert_no_alloc::assert_no_alloc(|| {
        drive(&mut inst, 256);
    });
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
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000),
            MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0),
        ];
        let events = Vst3InputEvents {
            midi: &warm,
            ..Default::default()
        };
        let _ = inst.process(&mut buffer, &events, None, &transport);
    }
    drive_silent(&mut inst, 32, &transport);

    let on_event = [MidiEvent::note_on(
        MidiGroup::FIRST,
        MidiChannel::FIRST,
        60,
        0x8000,
    )];
    let off_event = [MidiEvent::note_off(
        MidiGroup::FIRST,
        MidiChannel::FIRST,
        60,
        0,
    )];

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
