//! RT-safety: `ClapActive::process` must not allocate on the audio thread — in
//! steady state, and in the awkward corners a real plugin reaches.
//!
//! The `#[global_allocator]` below is what detects; `assert_no_alloc` only sets
//! a thread-local flag, so **without the allocator registered the gate is a
//! silent no-op that passes unconditionally**. A violation aborts the process,
//! so a failure here is a SIGABRT naming the test, not an assertion message.
//!
//! Two rules keep these non-vacuous, and every test below obeys them: the
//! hazardous work happens *inside* the gate (not the setup, not a warm-up that
//! primes it), and each test drives a plugin switch that *provokes* the corner
//! rather than the default configuration.
//!
//! This says nothing about the `RtPublish` deallocation property, which is a
//! race no allocation-sampling test can pin. Likewise the `clap.log` mutex: its
//! allocations are gated below, but a priority inversion is invisible here and
//! is handled by construction — the host refuses to reach the lock from the
//! audio thread, which the same test pins from the other side by asserting the
//! lines were counted as dropped.

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

/// The RT-hazard switches are process-global (one loaded image per test
/// process), so the whole set-switches → drive → assert sequence is serialized.
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
// loaded, not on the copy statically linked into this test binary. Those are
// two different sets of globals; the loader dedupes by path, so `libloading`
// against the same path reaches the host's image.
// ---------------------------------------------------------------------------

// `status::*` re-exports the CLAP status values so this suite cannot get them
// wrong: CLAP numbers ERROR = 0 and CONTINUE = 1, the reverse of the usual C
// convention.
use tutti_clap_test_plugin::rt_probe::status;
use tutti_clap_test_plugin::{StatusMode, WideLayout};

/// The plugin image, opened once and **never closed**.
///
/// A `OnceLock` rather than a per-call `Library::new` because dropping a
/// `libloading::Library` calls `dlclose`. [`set_wide_layout`] must run *before*
/// the host loads (the port layout is read once during load), so there the
/// test's handle is the only one: the switch writes an atomic, `dlclose` unmaps
/// the image, and the write is discarded with the mapping. Leaking one image
/// removes the ordering hazard.
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

fn set_audio_thread_log_lines(count: u32) {
    set_switch_u32(b"tutti_test_plugin_set_audio_thread_log_lines\0", count);
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
// Buffer plumbing. The reference plugin is 2-in / 2-out. Everything here is
// stack storage so that buffer setup inside the gate is slice reborrows only —
// a `Vec` of channels built per iteration would allocate and the test would be
// measuring itself.
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
// Baseline
// ===========================================================================

/// Steady-state `process` with no events must not allocate.
///
/// First so a regression can be localised: if this fails, the problem is the
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
// Process-status transitions.
//
// The host logged TAIL/SLEEP/unknown transitions with `eprintln!` from inside
// `do_process`, taking the stderr lock on the audio thread; the unknown-status
// arm additionally heap-formats an `i32`. The `status != prev_status` guard
// made this look rare — a plugin alternating between two statuses transitions
// on every block.
// ===========================================================================

/// A plugin alternating CONTINUE/TAIL must not make the host allocate.
///
/// Every block is a transition, so the host's guard passes every block. Not a
/// contrived plugin: a reverb whose tail decays below the noise floor and is
/// re-excited by fresh input reports exactly this.
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
    // Warm up with the switch already set, so the first transition is outside
    // the gate and the gate measures only steady alternation.
    drive(&mut inst, &mut bufs, 8, &ctx).expect("TAIL is not an error status");

    assert_no_alloc::assert_no_alloc(|| {
        drive(&mut inst, &mut bufs, 128, &ctx).expect("TAIL is not an error status");
    });

    // Not just "did not allocate" — the host must still have *observed* the
    // status, or a fix that deleted the tracking would pass. 128 blocks from an
    // even index leaves the last odd, so the final status is TAIL.
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
/// message. CLAP leaves the status space open, so an unknown value from a
/// plugin built against a newer header is legal, not misbehaviour.
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
/// Not a no-alloc test — a correctness one. `last_process_status` was written
/// every block but had no public reader, so the only thing the host did with a
/// TAIL was print it from the audio thread, where a caller deciding when to
/// stop driving a decaying plugin could not see it.
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
// Error construction on the audio thread.
// ===========================================================================

/// A plugin returning `CLAP_PROCESS_ERROR` every block must not make the host
/// allocate.
///
/// The host built `ClapError::ProcessError(...to_string())` in the callback,
/// and a plugin in an error state returns ERROR every block, so this allocated
/// per block. The error is asserted rather than merely tolerated: the host must
/// still report the failure, and still zero the output so undefined plugin
/// audio does not leak.
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

    // A clean block first, so the pools are warm and the gate sees only the
    // error path.
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
/// `String` before returning, and a host driving a too-large block drives it
/// again next block. The rejection itself is the point of the guard (an
/// oversized block would make the plugin write past the scratch), so this
/// asserts the error is still raised and still carries both numbers.
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

    // The rejection is real, and the numbers survived the move off the `String`.
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
// SysEx output events.
// ===========================================================================

/// A plugin emitting SysEx output events every block must not make the host
/// allocate.
///
/// `output_events_try_push` copied each payload with `to_vec()`, and the plugin
/// calls `try_push` from inside `process`. `process` also clears the output list
/// at the top of each block, dropping every payload `Vec` — so there was
/// nothing to amortise against and the same event allocated and freed per block.
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

    // Warm up with SysEx already flowing: the payload pool is fed by `clear`, so
    // the first emitting block legitimately grows it. Every block after must
    // recycle.
    set_sysex_output(EVENTS_PER_BLOCK, PAYLOAD_BYTES);
    drive(&mut inst, &mut bufs, 8, &ctx).expect("process");

    assert_no_alloc::assert_no_alloc(|| {
        drive(&mut inst, &mut bufs, 256, &ctx).expect("process");
    });
}

/// `ensure_processing` raising `StartProcessingFailed` must not allocate.
///
/// Asserted on the *variant* rather than end-to-end: reaching this path needs a
/// `ClapActive` whose plugin is deactivated, which the rollback in `reconfigure`
/// makes unreachable by design. It matters because `ensure_processing` runs
/// inside `do_process`, and nothing marks a refusing instance unusable — every
/// block re-attempts the call.
#[test]
fn start_processing_failure_is_allocation_free() {
    let err = assert_no_alloc::assert_no_alloc(|| {
        std::hint::black_box(tutti_clap_host::ClapError::StartProcessingFailed)
    });
    assert!(
        matches!(err, tutti_clap_host::ClapError::StartProcessingFailed),
        "constructing the variant must not allocate"
    );
    // Rendering happens off the audio thread, where the message is actually
    // read — so the text lives in the `#[error]` attribute, not in the value.
    assert_eq!(
        err.to_string(),
        "Processing error: plugin refused to start processing"
    );
}

/// A plugin logging through `clap.log` from inside `process` must not make the
/// host allocate, lock stderr, or take the log mutex.
///
/// CLAP marks `clap.log` `[thread-safe]`, which includes the audio thread. The
/// host used to treat such a line like a main-thread one: `into_owned()`,
/// `eprintln!`, and `LogState::push` taking a `Mutex` that `drain_log` holds
/// across a `.collect()`. The first two are allocations; the third is a
/// priority inversion this gate cannot see, caught instead by the host refusing
/// to reach the lock from this thread.
///
/// `log_lines_dropped` is asserted to move, so a "fix" that silently discarded
/// audio-thread logs would fail here rather than pass.
#[test]
fn audio_thread_logging_does_not_allocate() {
    let _lock = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    reset_probe();
    let mut inst = load_probe();
    let mut bufs = StereoBufs::new();
    let transport = TransportInfo::default();
    let ctx = ProcessContext {
        transport: Some(&transport),
        ..Default::default()
    };

    const LINES_PER_BLOCK: u32 = 4;
    const GATED_BLOCKS: u32 = 256;
    set_audio_thread_log_lines(LINES_PER_BLOCK);

    // A few blocks outside the gate for the host's one-off post-activation setup.
    drive(&mut inst, &mut bufs, 8, &ctx).expect("process");
    let before = inst.log_lines_dropped();

    assert_no_alloc::assert_no_alloc(|| {
        drive(&mut inst, &mut bufs, GATED_BLOCKS as usize, &ctx).expect("process");
    });

    let after = inst.log_lines_dropped();
    assert_eq!(
        after - before,
        LINES_PER_BLOCK * GATED_BLOCKS,
        "every audio-thread line must be counted as dropped — a host that \
         silently swallowed them would leave this at 0, and one that recorded \
         them would have allocated inside the gate"
    );
}

/// The same, with payloads that *vary* in size block to block.
///
/// A pool keyed on "the buffer I had last time" is only obviously sufficient
/// when every payload is identical, and real SysEx traffic is not. The four
/// sizes all sit below the largest warmed size so recycling is possible — a
/// larger one would legitimately grow a buffer, and demanding otherwise would
/// demand the host preallocate for an unbounded payload.
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
    // capacity the rotation needs. Warming smaller and stepping up would
    // allocate inside the gate — correctly, so the test would be asserting
    // something the host cannot promise.
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
/// `dlsym` itself allocates, and [`set_sysex_output`] resolves the symbol on
/// every call — fine outside a gate, fatal inside one, where the test would
/// abort on its own harness rather than on the host. Caching the raw `fn`
/// pointer leaves only the two relaxed atomic stores inside the gate.
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
// Wide channel layouts.
//
// The host collected the caller's channel pointers into `SmallVec<[*mut T; 16]>`
// locals, one per side, which spill to the heap at 17+ channels a side — every
// block, both sides. These layouts are supported configurations, not abuse: the
// host advertises surround and ambisonic port types precisely so it can be
// handed them. 7.1.4 Atmos is 12 channels; third-order ambisonic is 16.
// ===========================================================================

/// Drive `iters` blocks through a plugin with `channels` channels a side, using
/// heap channel storage allocated **outside** the gate.
///
/// The `&mut [&mut [f32]]` fan-out arrays are built once, before the gate, and
/// the gate only reborrows them — building them per block is what the host is
/// being tested not to do.
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

    // ONCE, before the gate: rebuilding per block would allocate in the harness
    // and mask the host.
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
/// Without it, the 20-channel test below could be passing because of anything
/// that scales with channel count. This pins the "just below the cliff" side.
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

    // Restore for whatever runs next: `reset_probe` does not cover this switch,
    // because a reset after loading would describe a layout the host is no
    // longer using.
    set_wide_layout(WideLayout::Off);
}

/// 20 channels a side is past the inline bound, so the unfixed host
/// heap-allocated two `SmallVec` backing buffers on every block.
///
/// 20 is what a 7.1.4 bed plus a discrete stereo pair presents.
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

/// The same spill reached through *port count* rather than one wide port: two
/// 12-channel ports a side, 24 channels. A fix that widened only the
/// single-port case passes the test above and fails this one.
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
