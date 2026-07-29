//! RT-safety regression: `ClapActive::process` must not allocate on the audio
//! thread — in steady state, **and** in the awkward corners a real plugin
//! reaches.
//!
//! # What this file replaced, and why it was worthless
//!
//! It previously drove TAL-NoiseMaker from a hard-coded
//! `/Library/Audio/Plug-Ins/CLAP/TAL-NoiseMaker.clap`, a macOS path, on a Linux
//! CI host — and both tests were `#[ignore]`d on top of that. Run the only way
//! the module docs suggested (`-- --ignored`), the loader found nothing,
//! returned `None`, and each test hit `let Some(..) else { return }` and
//! reported `ok`. So the suite announced **"2 passed"** for a plugin it had
//! never opened, on a platform where the path cannot exist. Every hazard below
//! was live the whole time this file claimed the process path was clean.
//!
//! It now drives the in-repo reference plugin, resolved by
//! [`support::probe_path`], which **panics** when the plugin is missing rather
//! than skipping — the plugin is a dev-dependency of this crate, so its absence
//! is a build failure, not a property of the machine. Nothing here is
//! `#[ignore]`d.
//!
//! # How the detection works
//!
//! The `#[global_allocator]` below is what detects; `assert_no_alloc` only sets
//! a thread-local flag, so **without the allocator registered the gate is a
//! silent no-op that passes unconditionally**. A violation aborts the process,
//! so a failure here is a SIGABRT naming the test, not a tidy assertion
//! message.
//!
//! # What "non-vacuous" means here, concretely
//!
//! A no-alloc test is trivially easy to write so that it cannot fail — this
//! project has already shipped one (`ClickSettings::meter`) that was
//! single-threaded and published outside the gate, so it passed whether or not
//! the bug was present. Two rules follow, and every test below obeys them:
//!
//! 1. **The hazardous work happens inside the gate.** Not the setup for it, not
//!    a warm-up that primes it — the thing that would allocate.
//! 2. **Each test was run against the unfixed host and observed to abort.** The
//!    reference plugin's switches exist so the hazard can be *provoked*, not
//!    merely tolerated: a test that drives the default configuration measures
//!    nothing about the corners.
//!
//! # A stale-artifact hazard these tests uncovered
//!
//! `build.rs` emits two candidate paths for the reference plugin —
//! `<profile>/<name>` and `<profile>/deps/<name>` — and `probe_path` returns
//! the **first that exists**. Under this workspace's shared external target
//! dir, both can exist, and cargo does not necessarily refresh the
//! `<profile>/` copy on every build: it was observed five minutes older than
//! the `deps/` one after a plugin edit.
//!
//! That is not cosmetic. While verifying non-vacuity, a deliberately-neutered
//! plugin switch was still reported as working, because the host had dlopened
//! the stale `<profile>/` copy — the suite was measuring a build that no longer
//! matched the source. Deleting the stale copy immediately turned five tests
//! red, which is the answer that should have come back the first time.
//!
//! `build.rs` and `tests/support/` are owned elsewhere, so this is recorded
//! rather than fixed here. A suite that mysteriously passes after a plugin-side
//! change should suspect this first: compare the mtimes of the two candidates
//! before believing the result.
//!
//! # What this file does not prove
//!
//! Only that the sampled paths do not call the allocator. It says nothing about
//! the `RtPublish` deallocation property, which is a race and cannot be pinned
//! by any allocation-sampling test (see the project's "Publishing to the Audio
//! Thread" policy) — and nothing about the `clap.log` mutex (H4), whose hazard
//! is a *lock*, not an allocation. Both are noted in the report rather than
//! claimed here.

use std::path::Path;
use std::sync::Mutex;

use assert_no_alloc::AllocDisabler;
use tutti_clap_host::{
    AudioBuffer32, ClapActive, ClapLoaded, MidiEvent, ProcessContext, TransportInfo,
};

mod support;
use support::probe_path::probe_path;

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// The reference plugin's RT-hazard switches are **process-global** (one loaded
/// image per test process), and `cargo test` runs these in parallel threads. A
/// test that set a status mode while another was mid-gate would change what the
/// other was measuring. Serialize the whole set-switches → drive → assert
/// sequence.
///
/// This also serializes plugin loads, which the previous file did for the same
/// reason.
static PROBE_LOCK: Mutex<()> = Mutex::new(());

/// Block size every test drives. Well under the 512 the instance is activated
/// for, so the C1 block-size guard is not in play except where a test aims at
/// it deliberately.
const FRAMES: usize = 64;
const SAMPLE_RATE: f64 = 48_000.0;
const MAX_FRAMES: u32 = 512;

// ---------------------------------------------------------------------------
// The reference plugin's control surface, reached across the dlopen seam.
//
// The plugin is also an rlib dev-dependency, so the constants come from the
// crate directly — but the *switches* must be called on the image the host
// loaded, not on a second copy statically linked into this test binary. Those
// are two different sets of globals. Hence `libloading` against the same path:
// the loader dedupes by path, so this reaches the host's image.
// ---------------------------------------------------------------------------

// `status::*` re-exports the CLAP-defined status values so this suite does not
// take its own `clap-sys` dependency — and, more usefully, so it cannot get
// them wrong: CLAP numbers ERROR = 0 and CONTINUE = 1, which is the reverse of
// the usual C convention.
use tutti_clap_test_plugin::rt_probe::status;
use tutti_clap_test_plugin::{StatusMode, WideLayout};

/// The plugin image, opened once and **never closed**.
///
/// This being a `OnceLock` rather than a per-call `Library::new` is load-bearing,
/// and the bug it fixes is worth naming because it is invisible: dropping a
/// `libloading::Library` calls `dlclose`. When the host already holds the image
/// open — which is true for every switch flipped *after* `load_probe()` — the
/// refcount stays positive and the drop is harmless. But
/// [`set_wide_layout`] must run **before** the host loads, because the audio
/// port layout is read once during load. There the test's own handle is the
/// only one: `Library::new` maps the image, the switch writes the atomic,
/// `dlclose` unmaps it, and the write is discarded along with the mapping. The
/// host then loads a fresh image whose layout switch is still at its default.
///
/// The symptom was a test asserting 16 channels and reading 2, with the plugin
/// side provably correct — the store had happened, to memory that no longer
/// existed.
///
/// Leaking one image for the lifetime of a test binary that keeps the plugin
/// loaded throughout costs nothing and removes the ordering hazard entirely.
fn probe_lib() -> &'static libloading::Library {
    use std::sync::OnceLock;
    static LIB: OnceLock<libloading::Library> = OnceLock::new();
    LIB.get_or_init(|| unsafe {
        libloading::Library::new(probe_path()).expect("re-open reference plugin")
    })
}

/// Call a `extern "C" fn(u32)` switch on the host's loaded plugin image.
fn set_switch_u32(symbol: &[u8], value: u32) {
    unsafe {
        let f: libloading::Symbol<unsafe extern "C" fn(u32)> =
            probe_lib().get(symbol).expect("switch symbol present");
        f(value);
    }
}

/// Call a `extern "C" fn(u32, u32)` switch on the host's loaded plugin image.
fn set_switch_u32_u32(symbol: &[u8], a: u32, b: u32) {
    unsafe {
        let f: libloading::Symbol<unsafe extern "C" fn(u32, u32)> =
            probe_lib().get(symbol).expect("switch symbol present");
        f(a, b);
    }
}

/// Call a nullary `extern "C" fn()` on the host's loaded plugin image.
fn call_switch(symbol: &[u8]) {
    unsafe {
        let f: libloading::Symbol<unsafe extern "C" fn()> =
            probe_lib().get(symbol).expect("switch symbol present");
        f();
    }
}

fn set_status_mode(mode: StatusMode) {
    set_switch_u32(b"tutti_test_plugin_set_status_mode\0", mode as u32);
}

fn set_sysex_output(count: u32, bytes: u32) {
    set_switch_u32_u32(b"tutti_test_plugin_set_sysex_output_bytes\0", count, bytes);
}

fn set_wide_layout(layout: WideLayout) {
    set_switch_u32(b"tutti_test_plugin_set_wide_layout\0", layout as u32);
}

/// Return every RT-probe switch to its inert default. Called on entry to each
/// test rather than on exit from the previous one, so a test that panics
/// mid-gate cannot poison its successors.
fn reset_probe() {
    call_switch(b"tutti_test_plugin_reset_rt_probe\0");
}

/// Load + activate the reference plugin.
///
/// Panics if it was not built — see [`support::probe_path`] for why that is the
/// only correct response.
fn load_probe() -> ClapActive<f32> {
    let path = Path::new(probe_path());
    // The artifact is a bare dylib, so it serves as both bundle and library —
    // the host dlopens it directly, no `.clap` bundle structure needed.
    let loaded = ClapLoaded::load_with_library(path, Some(path), SAMPLE_RATE, MAX_FRAMES)
        .expect("reference plugin should load");
    loaded
        .activate::<f32>()
        .map_err(|(_, e)| e)
        .expect("reference plugin should activate")
}

// ---------------------------------------------------------------------------
// Buffer plumbing.
//
// The reference plugin is 2-in / 2-out (`PortLayoutMode::SymmetricStereo`),
// unlike TAL-NoiseMaker's 0-in / 2-out synth shape the old file hard-coded.
// Everything here is stack storage so that the buffer setup inside the gate is
// slice reborrows only — a `Vec` of channels built per iteration would allocate
// and the test would be measuring itself.
// ---------------------------------------------------------------------------

/// Stack channel storage for a stereo-in / stereo-out block.
struct StereoBufs {
    in_l: [f32; FRAMES],
    in_r: [f32; FRAMES],
    out_l: [f32; FRAMES],
    out_r: [f32; FRAMES],
}

impl StereoBufs {
    fn new() -> Self {
        Self {
            in_l: [0.0; FRAMES],
            in_r: [0.0; FRAMES],
            out_l: [0.0; FRAMES],
            out_r: [0.0; FRAMES],
        }
    }
}

/// Run `iters` blocks through the plugin, returning the last `process` result
/// so a caller can assert on the error path.
///
/// Buffer setup is per-iteration but stack-only: two slice reborrows and a
/// struct literal. No heap touch, so the loop can live inside the gate.
fn drive(
    inst: &mut ClapActive<f32>,
    bufs: &mut StereoBufs,
    iters: usize,
    ctx: &ProcessContext<'_>,
) -> Result<(), tutti_clap_host::ClapError> {
    let mut last = Ok(());
    for _ in 0..iters {
        let outs: &mut [&mut [f32]] = &mut [&mut bufs.out_l[..], &mut bufs.out_r[..]];
        let ins: &[&[f32]] = &[&bufs.in_l[..], &bufs.in_r[..]];
        let mut buffer = AudioBuffer32 {
            inputs: ins,
            outputs: outs,
            num_samples: FRAMES,
            sample_rate: SAMPLE_RATE,
        };
        last = inst.process(&mut buffer, ctx).map(|_| ());
    }
    last
}

// ===========================================================================
// Baseline — the property the old file meant to assert.
// ===========================================================================

/// Steady-state `process` with no events must not allocate.
///
/// The weakest of the tests here, and the only one the old file attempted. Kept
/// first so a regression can be localised: if this fails, the problem is the
/// common path rather than any of the hazard corners below.
#[test]
fn process_steady_state_does_not_allocate() {
    let _lock = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    reset_probe();
    let mut inst = load_probe();
    let mut bufs = StereoBufs::new();
    let transport = TransportInfo::default();
    let ctx = ProcessContext {
        transport: Some(&transport),
        ..Default::default()
    };

    // Warm up outside the gate: `start_processing` runs on the first block, and
    // the pooled scratch was sized at activation but not yet touched. Those are
    // one-shot costs, not the per-block property under test.
    drive(&mut inst, &mut bufs, 32, &ctx).expect("warm-up blocks should succeed");

    assert_no_alloc::assert_no_alloc(|| {
        drive(&mut inst, &mut bufs, 256, &ctx).expect("steady-state blocks should succeed");
    });
}

/// Steady-state `process` **with input MIDI** must not allocate.
///
/// Separate from the block above because the event path has its own pools
/// (`input_events`, and the `out_*` return pools drained after the call), and a
/// regression in those would not show up in a silent block.
#[test]
fn process_with_midi_does_not_allocate() {
    let _lock = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    reset_probe();
    let mut inst = load_probe();
    let mut bufs = StereoBufs::new();
    let transport = TransportInfo::default();

    let on_event = [MidiEvent::note_on(0, 0, 60, 0x8000)];
    let off_event = [MidiEvent::note_off(0, 0, 60, 0)];

    // Prime with a note pair, then silent blocks, so any first-call lazy
    // allocation on the event path is flushed before the gate.
    {
        let warm = [on_event[0], off_event[0]];
        let ctx = ProcessContext {
            midi: &warm,
            transport: Some(&transport),
            ..Default::default()
        };
        drive(&mut inst, &mut bufs, 1, &ctx).expect("warm-up block should succeed");
    }
    let silent = ProcessContext {
        transport: Some(&transport),
        ..Default::default()
    };
    drive(&mut inst, &mut bufs, 32, &silent).expect("warm-up blocks should succeed");

    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..128usize {
            let events: &[MidiEvent] = match i % 32 {
                0 => &on_event,
                16 => &off_event,
                _ => &[],
            };
            let ctx = ProcessContext {
                midi: events,
                transport: Some(&transport),
                ..Default::default()
            };
            let outs: &mut [&mut [f32]] = &mut [&mut bufs.out_l[..], &mut bufs.out_r[..]];
            let ins: &[&[f32]] = &[&bufs.in_l[..], &bufs.in_r[..]];
            let mut buffer = AudioBuffer32 {
                inputs: ins,
                outputs: outs,
                num_samples: FRAMES,
                sample_rate: SAMPLE_RATE,
            };
            inst.process(&mut buffer, &ctx).expect("process");
        }
    });
}

// ===========================================================================
// H1 — process-status transitions.
//
// The host logged TAIL/SLEEP/unknown transitions with `eprintln!` from inside
// `do_process`, under an `AudioThreadClaim`. That takes the stderr lock on the
// audio thread, and the unknown-status arm additionally heap-formats an `i32`.
//
// The `status != prev_status` guard made this look rare. It is not: a plugin
// that alternates between two statuses transitions on every single block.
// ===========================================================================

/// A plugin alternating CONTINUE/TAIL must not make the host allocate.
///
/// **This is the H1 test.** Every block is a transition, so the host's guard
/// passes every block and the old code reached its `eprintln!` every block.
///
/// Alternating CONTINUE/TAIL is not a contrived plugin: a reverb whose tail
/// decays below the noise floor and is re-excited by fresh input reports
/// exactly this.
#[test]
fn alternating_continue_tail_does_not_allocate() {
    let _lock = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    reset_probe();
    let mut inst = load_probe();
    let mut bufs = StereoBufs::new();
    let transport = TransportInfo::default();
    let ctx = ProcessContext {
        transport: Some(&transport),
        ..Default::default()
    };

    set_status_mode(StatusMode::AlternateContinueTail);
    // Warm up *with the switch already set*, so the first transition — which is
    // as much a one-shot as any other first call — is outside the gate and the
    // gate measures only steady alternation.
    drive(&mut inst, &mut bufs, 8, &ctx).expect("TAIL is not an error status");

    assert_no_alloc::assert_no_alloc(|| {
        drive(&mut inst, &mut bufs, 128, &ctx).expect("TAIL is not an error status");
    });

    // The property is not just "did not allocate" — the host must still have
    // *observed* the status, or a fix that simply deleted the tracking would
    // pass. 128 blocks starting from an even index leaves the last one odd, so
    // the final status is TAIL.
    assert!(
        inst.is_tailing(),
        "the host must still record TAIL — it is now the only route by which a \
         caller can see it, since the eprintln is gone. last_process_status = {}",
        inst.last_process_status()
    );
}

/// A plugin returning a status CLAP does not define must not make the host
/// allocate.
///
/// Reaches the host's `other =>` arm, which formatted the raw `i32` into a
/// message — an allocation on top of the stderr lock. CLAP leaves the status
/// space open, so a plugin built against a newer header returning an unknown
/// value is legal, not misbehaviour.
#[test]
fn alternating_unknown_status_does_not_allocate() {
    let _lock = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    reset_probe();
    let mut inst = load_probe();
    let mut bufs = StereoBufs::new();
    let transport = TransportInfo::default();
    let ctx = ProcessContext {
        transport: Some(&transport),
        ..Default::default()
    };

    set_status_mode(StatusMode::AlternateContinueGarbage);
    drive(&mut inst, &mut bufs, 8, &ctx).expect("an unknown status is not an error status");

    assert_no_alloc::assert_no_alloc(|| {
        drive(&mut inst, &mut bufs, 128, &ctx).expect("an unknown status is not an error status");
    });

    assert_eq!(
        inst.last_process_status(),
        tutti_clap_test_plugin::GARBAGE_STATUS,
        "the host must report the plugin's status verbatim rather than folding \
         an unrecognised value into a known bucket"
    );
}

/// SLEEP alternation, for the same reason — so a fix that special-cases only
/// TAIL is still caught.
#[test]
fn alternating_continue_sleep_does_not_allocate() {
    let _lock = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    reset_probe();
    let mut inst = load_probe();
    let mut bufs = StereoBufs::new();
    let transport = TransportInfo::default();
    let ctx = ProcessContext {
        transport: Some(&transport),
        ..Default::default()
    };

    set_status_mode(StatusMode::AlternateContinueSleep);
    drive(&mut inst, &mut bufs, 8, &ctx).expect("SLEEP is not an error status");

    assert_no_alloc::assert_no_alloc(|| {
        drive(&mut inst, &mut bufs, 128, &ctx).expect("SLEEP is not an error status");
    });

    assert!(
        inst.is_sleeping(),
        "the host must still record SLEEP; last_process_status = {}",
        inst.last_process_status()
    );
}

/// TAIL and SLEEP must be reachable by a caller at all.
///
/// Not a no-alloc test — a correctness one, and the reason the accessor exists.
/// `last_process_status` was written on every block but had no public reader,
/// so the doc claiming callers could observe TAIL/SLEEP through it described
/// something no caller could do. The only thing the host actually did with a
/// TAIL was print it, from the audio thread, where the information was
/// unavailable to the code that needed it (a host deciding when to stop calling
/// a decaying plugin).
#[test]
fn tail_and_sleep_are_observable_by_a_caller() {
    let _lock = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    reset_probe();
    let mut inst = load_probe();
    let mut bufs = StereoBufs::new();
    let transport = TransportInfo::default();
    let ctx = ProcessContext {
        transport: Some(&transport),
        ..Default::default()
    };

    // Default: CONTINUE, and neither predicate fires.
    drive(&mut inst, &mut bufs, 4, &ctx).expect("process");
    assert!(!inst.is_tailing() && !inst.is_sleeping());
    assert_eq!(
        inst.last_process_status(),
        status::CONTINUE,
        "steady CONTINUE"
    );

    set_status_mode(StatusMode::Tail);
    drive(&mut inst, &mut bufs, 1, &ctx).expect("process");
    assert!(inst.is_tailing(), "TAIL must be observable");
    assert!(!inst.is_sleeping());

    set_status_mode(StatusMode::Sleep);
    drive(&mut inst, &mut bufs, 1, &ctx).expect("process");
    assert!(inst.is_sleeping(), "SLEEP must be observable");
    assert!(!inst.is_tailing());

    set_status_mode(StatusMode::ContinueIfNotQuiet);
    drive(&mut inst, &mut bufs, 1, &ctx).expect("process");
    assert!(
        !inst.is_tailing() && !inst.is_sleeping(),
        "CONTINUE_IF_NOT_QUIET is neither tailing nor sleeping"
    );
}

// ===========================================================================
// H2 — error construction on the audio thread.
// ===========================================================================

/// A plugin returning `CLAP_PROCESS_ERROR` every block must not make the host
/// allocate.
///
/// The host built `ClapError::ProcessError("Plugin returned error".to_string())`
/// — a heap allocation, on the audio thread, in the callback. And a plugin in
/// an error state does not return ERROR once: it returns it every block, so
/// this allocated per block on precisely the path where the host had already
/// concluded something was wrong.
///
/// The error is asserted, not merely tolerated: the host must still *report*
/// the failure, and it must still zero the output so undefined plugin audio
/// does not leak.
#[test]
fn plugin_error_status_does_not_allocate() {
    let _lock = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    reset_probe();
    let mut inst = load_probe();
    let mut bufs = StereoBufs::new();
    let transport = TransportInfo::default();
    let ctx = ProcessContext {
        transport: Some(&transport),
        ..Default::default()
    };

    // A clean block first, so `start_processing` and the pools are warm and the
    // gate sees only the error path.
    drive(&mut inst, &mut bufs, 8, &ctx).expect("clean warm-up");

    set_status_mode(StatusMode::Error);
    // Dirty the outputs so "the host zeroed them" is distinguishable from
    // "nothing ever wrote to them".
    bufs.out_l.fill(1.25);
    bufs.out_r.fill(-3.5);

    let err = drive(&mut inst, &mut bufs, 1, &ctx)
        .expect_err("the host must surface CLAP_PROCESS_ERROR as an error");
    assert!(
        matches!(err, tutti_clap_host::ClapError::PluginReturnedError),
        "expected the allocation-free error variant, got {err:?}"
    );
    assert!(
        bufs.out_l.iter().all(|&s| s == 0.0) && bufs.out_r.iter().all(|&s| s == 0.0),
        "on ERROR the plugin's output is undefined, so the host must zero it \
         rather than let garbage through"
    );

    // Now the property: 128 further error blocks, all inside the gate.
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..128 {
            let outs: &mut [&mut [f32]] = &mut [&mut bufs.out_l[..], &mut bufs.out_r[..]];
            let ins: &[&[f32]] = &[&bufs.in_l[..], &bufs.in_r[..]];
            let mut buffer = AudioBuffer32 {
                inputs: ins,
                outputs: outs,
                num_samples: FRAMES,
                sample_rate: SAMPLE_RATE,
            };
            // The error is expected; what must not happen is an allocation
            // while producing it.
            let _ = inst.process(&mut buffer, &ctx);
        }
    });
}

/// Rejecting an oversized block must not allocate.
///
/// The C1 guard formatted the requested and activated frame counts into a
/// `String` — on the audio thread, before returning. A host driving a
/// too-large block drives it again next block, so the allocation repeats.
///
/// The rejection itself is the point of the guard (an oversized block would
/// make the plugin write past the scratch), so this asserts the error is still
/// raised and still carries both numbers.
#[test]
fn oversized_block_rejection_does_not_allocate() {
    let _lock = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    reset_probe();
    let mut inst = load_probe();
    let transport = TransportInfo::default();
    let ctx = ProcessContext {
        transport: Some(&transport),
        ..Default::default()
    };

    // Oversized relative to MAX_FRAMES (512). Stack storage, as everywhere here.
    const OVERSIZE: usize = 1024;
    let mut in_l = [0.0f32; OVERSIZE];
    let mut in_r = [0.0f32; OVERSIZE];
    let mut out_l = [0.0f32; OVERSIZE];
    let mut out_r = [0.0f32; OVERSIZE];
    in_l.fill(0.0);
    in_r.fill(0.0);

    // Warm up on a legal block so the guard is the only thing the gate sees.
    {
        let mut bufs = StereoBufs::new();
        drive(&mut inst, &mut bufs, 8, &ctx).expect("legal warm-up");
    }

    // Assert the rejection is real, and that the numbers survived the move off
    // the `String`.
    {
        let outs: &mut [&mut [f32]] = &mut [&mut out_l[..], &mut out_r[..]];
        let ins: &[&[f32]] = &[&in_l[..], &in_r[..]];
        let mut buffer = AudioBuffer32 {
            inputs: ins,
            outputs: outs,
            num_samples: OVERSIZE,
            sample_rate: SAMPLE_RATE,
        };
        let err = inst
            .process(&mut buffer, &ctx)
            .expect_err("a block past max_frames must be rejected, not truncated");
        match err {
            tutti_clap_host::ClapError::BlockTooLarge {
                requested,
                max_frames,
            } => {
                assert_eq!(requested, OVERSIZE as u32);
                assert_eq!(max_frames, MAX_FRAMES);
            }
            other => panic!("expected the allocation-free BlockTooLarge, got {other:?}"),
        }
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..128 {
            let outs: &mut [&mut [f32]] = &mut [&mut out_l[..], &mut out_r[..]];
            let ins: &[&[f32]] = &[&in_l[..], &in_r[..]];
            let mut buffer = AudioBuffer32 {
                inputs: ins,
                outputs: outs,
                num_samples: OVERSIZE,
                sample_rate: SAMPLE_RATE,
            };
            let _ = inst.process(&mut buffer, &ctx);
        }
    });
}

// ===========================================================================
// H3 — SysEx output events.
// ===========================================================================

/// A plugin emitting SysEx output events every block must not make the host
/// allocate.
///
/// `output_events_try_push` copied each payload with `to_vec()` — and the
/// plugin calls `try_push` from *inside* `process`, so that allocation lands on
/// the audio thread. Worse, `process` clears the output list at the top of each
/// block, which dropped every payload `Vec`; there was nothing to amortise
/// against, so a plugin emitting the same event every block allocated and freed
/// every block.
///
/// The events are asserted to arrive, so a "fix" that dropped SysEx on the
/// floor would fail here rather than pass quietly.
#[test]
fn sysex_output_events_do_not_allocate() {
    let _lock = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    reset_probe();
    let mut inst = load_probe();
    let mut bufs = StereoBufs::new();
    let transport = TransportInfo::default();
    let ctx = ProcessContext {
        transport: Some(&transport),
        ..Default::default()
    };

    const EVENTS_PER_BLOCK: u32 = 4;
    const PAYLOAD_BYTES: u32 = 200;

    // Warm up with SysEx already flowing: the payload pool is fed by `clear`,
    // so the very first emitting block legitimately grows it. That is a one-off
    // and belongs outside the gate; every block after it must recycle.
    set_sysex_output(EVENTS_PER_BLOCK, PAYLOAD_BYTES);
    drive(&mut inst, &mut bufs, 8, &ctx).expect("process");

    assert_no_alloc::assert_no_alloc(|| {
        drive(&mut inst, &mut bufs, 256, &ctx).expect("process");
    });
}

/// The same, with payloads that *vary* in size block to block.
///
/// A pool keyed on "the buffer I had last time" is only obviously sufficient
/// when every payload is identical. Real SysEx traffic is not: a device replies
/// to an identity request, then dumps a patch. This drives four distinct sizes
/// through the pool in rotation, all below the largest warmed size so recycling
/// is possible — a size *larger* than anything seen would legitimately grow a
/// buffer, and demanding otherwise would be demanding the host preallocate for
/// an unbounded payload.
#[test]
fn varying_sysex_payload_sizes_do_not_allocate() {
    let _lock = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    reset_probe();
    let mut inst = load_probe();
    let mut bufs = StereoBufs::new();
    let transport = TransportInfo::default();
    let ctx = ProcessContext {
        transport: Some(&transport),
        ..Default::default()
    };

    const SIZES: [u32; 4] = [256, 8, 130, 64];
    const EVENTS_PER_BLOCK: u32 = 3;

    // Warm up at the LARGEST size first, so every pooled buffer reaches the
    // capacity the rotation will need. Warming at a smaller size and then
    // stepping up would allocate inside the gate — correctly, and the test
    // would be asserting something the host cannot promise.
    set_sysex_output(EVENTS_PER_BLOCK, SIZES[0]);
    drive(&mut inst, &mut bufs, 8, &ctx).expect("process");
    for size in SIZES {
        set_sysex_output(EVENTS_PER_BLOCK, size);
        drive(&mut inst, &mut bufs, 2, &ctx).expect("process");
    }

    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..128usize {
            // The switch is a pair of relaxed atomic stores — no allocation, so
            // it is safe to flip inside the gate, and flipping it inside is
            // what makes the rotation real rather than a fixed size repeated.
            set_sysex_output_no_alloc(EVENTS_PER_BLOCK, SIZES[i % SIZES.len()]);
            drive(&mut inst, &mut bufs, 1, &ctx).expect("process");
        }
    });
}

/// The SysEx switch, through a function pointer resolved **once** outside the
/// gate.
///
/// `dlsym` itself allocates. [`set_sysex_output`] resolves the symbol on every
/// call, which is fine outside a gate and fatal inside one — the test would
/// abort on its own harness rather than on the host, which is the most
/// misleading possible failure. Caching the raw `fn` pointer leaves only the
/// two relaxed atomic stores inside the gate.
fn set_sysex_output_no_alloc(count: u32, bytes: u32) {
    use std::sync::OnceLock;
    static F: OnceLock<unsafe extern "C" fn(u32, u32)> = OnceLock::new();
    let f = *F.get_or_init(|| unsafe {
        let sym: libloading::Symbol<unsafe extern "C" fn(u32, u32)> = probe_lib()
            .get(b"tutti_test_plugin_set_sysex_output_bytes\0")
            .expect("switch symbol present");
        *sym
    });
    unsafe { f(count, bytes) }
}

// ===========================================================================
// H7 — wide channel layouts.
//
// The host collected the caller's channel pointers into
// `SmallVec<[*mut T; 16]>` locals, one per side. At 17+ channels a side that
// spills to the heap — every block, both sides.
//
// The layouts below are supported configurations, not abuse: the host
// advertises surround and ambisonic port types precisely so it can be handed
// them. 7.1.4 Atmos is 12 channels; two 8-channel ports is 16; third-order
// ambisonic is 16.
// ===========================================================================

/// Drive `iters` blocks through a plugin with `channels` channels a side, using
/// heap channel storage allocated **outside** the gate.
///
/// The channel storage has to be heap — 20 channels of stack array per side
/// times two is fine, but the `&mut [&mut [f32]]` fan-out array itself must be
/// built somewhere, and building it per block is what the host is being tested
/// not to do. So the pointer arrays are built once, before the gate, and the
/// gate only reborrows them.
fn drive_wide(inst: &mut ClapActive<f32>, channels: usize, iters: usize) {
    let transport = TransportInfo::default();
    let ctx = ProcessContext {
        transport: Some(&transport),
        ..Default::default()
    };

    // Backing storage, allocated up front.
    let ins_storage: Vec<Vec<f32>> = (0..channels).map(|_| vec![0.0f32; FRAMES]).collect();
    let mut outs_storage: Vec<Vec<f32>> = (0..channels).map(|_| vec![0.0f32; FRAMES]).collect();

    // Warm up outside the gate.
    for _ in 0..8 {
        let ins: Vec<&[f32]> = ins_storage.iter().map(|v| v.as_slice()).collect();
        let mut outs: Vec<&mut [f32]> = outs_storage.iter_mut().map(|v| v.as_mut_slice()).collect();
        let mut buffer = AudioBuffer32 {
            inputs: &ins,
            outputs: &mut outs,
            num_samples: FRAMES,
            sample_rate: SAMPLE_RATE,
        };
        inst.process(&mut buffer, &ctx).expect("warm-up process");
    }

    // Build the borrow arrays ONCE, before the gate. Rebuilding them per block
    // inside the gate would allocate in the harness and mask the host.
    let ins: Vec<&[f32]> = ins_storage.iter().map(|v| v.as_slice()).collect();
    let mut outs: Vec<&mut [f32]> = outs_storage.iter_mut().map(|v| v.as_mut_slice()).collect();

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..iters {
            let mut buffer = AudioBuffer32 {
                inputs: &ins,
                outputs: &mut outs,
                num_samples: FRAMES,
                sample_rate: SAMPLE_RATE,
            };
            inst.process(&mut buffer, &ctx).expect("process");
        }
    });
}

/// **The control case.** Exactly 16 channels a side is still inline in a
/// `SmallVec<[T; 16]>`, so this passed even against the unfixed host.
///
/// It is here on purpose. Without it, the 20-channel test below could be
/// passing because of anything that scales with channel count, and there would
/// be no evidence the boundary is where the analysis says it is. This pins the
/// "just below the cliff" side; the next test pins "just past it".
#[test]
fn exactly_sixteen_channels_per_side_does_not_allocate() {
    let _lock = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    reset_probe();
    // The layout is read at load time, so it must be set BEFORE loading.
    set_wide_layout(WideLayout::Exactly16);
    let mut inst = load_probe();
    assert_eq!(
        inst.num_input_channels(),
        16,
        "the host must see the wide layout the plugin reported"
    );

    drive_wide(&mut inst, 16, 128);

    // Restore for whatever runs next: this switch is not covered by
    // `reset_probe`, because a reset after loading would describe a layout the
    // host is no longer using.
    set_wide_layout(WideLayout::Off);
}

/// **The H7 test.** 20 channels a side is past the inline bound, so the unfixed
/// host heap-allocated two `SmallVec` backing buffers on every block.
///
/// 20 is not an arbitrary number past 16: it is what a 7.1.4 bed plus a
/// discrete stereo pair, or a 4th-order-adjacent ambisonic layout, presents.
#[test]
fn twenty_channels_per_side_does_not_allocate() {
    let _lock = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    reset_probe();
    set_wide_layout(WideLayout::Wide20);
    let mut inst = load_probe();
    assert_eq!(
        inst.num_input_channels(),
        20,
        "the host must see the wide layout the plugin reported"
    );

    drive_wide(&mut inst, 20, 128);

    set_wide_layout(WideLayout::Off);
}

/// The same spill reached through *port count* rather than one wide port:
/// two 12-channel ports a side, i.e. 7.1.4 Atmos twice over, for 24 channels.
///
/// Separate from the test above because the two arrive at the same total by
/// different routes, and a fix that widened only the single-port case would
/// pass one and fail the other.
#[test]
fn two_twelve_channel_ports_per_side_do_not_allocate() {
    let _lock = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    reset_probe();
    set_wide_layout(WideLayout::Split12Plus12);
    let mut inst = load_probe();
    assert_eq!(
        inst.num_input_channels(),
        24,
        "two 12-channel ports must sum to 24 on the host side"
    );

    drive_wide(&mut inst, 24, 128);

    set_wide_layout(WideLayout::Off);
}
