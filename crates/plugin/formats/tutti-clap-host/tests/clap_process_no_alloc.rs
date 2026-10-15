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
    AudioBuffer32, ClapActive, ClapLoaded, ClapProcessContext, MidiEvent, TransportInfo,
};

mod support;
use support::probe_path::probe_path;

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// The RT-hazard switches are process-global (one loaded image per test
/// process), so the whole set-switches → drive → assert sequence is serialized.
static PROBE_LOCK: Mutex<()> = Mutex::new(());

/// Block size every test drives. Well under the 512 the instance is activated
/// for, so the block-size guard is not in play except where a test aims at
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
use tutti_midi_types::{MidiChannel, MidiGroup};

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

fn set_param_output(params: u32, points: u32) {
    set_switch_u32_u32(b"tutti_test_plugin_set_param_output\0", params, points);
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
    ctx: &ClapProcessContext<'_>,
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
    let ctx = ClapProcessContext {
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

    // Prime with a note pair, then silent blocks, so any first-call lazy
    // allocation on the event path is flushed before the gate.
    {
        let warm = [on_event[0], off_event[0]];
        let ctx = ClapProcessContext {
            midi: &warm,
            transport: Some(&transport),
            ..Default::default()
        };
        drive(&mut inst, &mut bufs, 1, &ctx).expect("warm-up block should succeed");
    }
    let silent = ClapProcessContext {
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
            let ctx = ClapProcessContext {
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
    let ctx = ClapProcessContext {
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
    // status, or a host that dropped the tracking would pass. 128 blocks from an
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
    let ctx = ClapProcessContext {
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
    let ctx = ClapProcessContext {
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
    let ctx = ClapProcessContext {
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
    let ctx = ClapProcessContext {
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
/// A guard that formatted the requested and activated frame counts into a
/// `String` would allocate on every such block, and a host driving a too-large
/// block drives it again next block. The rejection itself is the point of the guard (an
/// oversized block would make the plugin write past the scratch), so this
/// asserts the error is still raised and still carries both numbers.
#[test]
fn oversized_block_rejection_does_not_allocate() {
    let _lock = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    reset_probe();
    let mut inst = load_probe();
    let transport = TransportInfo::default();
    let ctx = ClapProcessContext {
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
    let ctx = ClapProcessContext {
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

// ===========================================================================
// Parameter output events.
// ===========================================================================

/// One block through the plugin, outside any gate, returning how many points
/// each output parameter queue holds and the value each ended on.
fn drive_and_read_param_output(
    inst: &mut ClapActive<f32>,
    bufs: &mut StereoBufs,
) -> Vec<(u32, usize, f64)> {
    drive_and_read_output(inst, bufs, &[]).0
}

/// [`drive_and_read_param_output`] with `midi` sent in, also returning how
/// many note-offs the plugin's output carried.
fn drive_and_read_output(
    inst: &mut ClapActive<f32>,
    bufs: &mut StereoBufs,
    midi: &[MidiEvent],
) -> (Vec<(u32, usize, f64)>, usize) {
    let transport = TransportInfo::default();
    let ctx = ClapProcessContext {
        midi,
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
    let out = inst.process(&mut buffer, &ctx).expect("process");
    let queues = out
        .param_changes
        .queues
        .iter()
        .map(|q| {
            let id = q.param_id.opaque().expect("a CLAP id is opaque").get();
            let last = q.points.last().expect("an open queue has a point");
            (id, q.points.len(), last.value.get())
        })
        .collect();
    let note_offs = out.midi_events.iter().filter(|e| e.is_note_off()).count();
    (queues, note_offs)
}

/// The output queues the host reserves at activation for the reference
/// plugin: its parameter count, floored at 16, which the probe stays under.
const OUTPUT_QUEUES: u32 = 16;

/// **A burst of plugin parameter output, within the room reserved at
/// activation, neither allocates nor frees.** Sixteen parameters with twelve
/// points each, every block: more than `ParameterQueue::MIN_POINTS`, the
/// least room a queue is reserved with, and a queue built per block would
/// allocate, and one dropped per block would free. The host groups them into the queues it reserved at activation
/// (16, each with `ParameterQueue::chunk_room(512)` = 20 points), takes each
/// back with `open_queue` and returns it with `clear`. The gate covers many
/// blocks, so the second and later reuse the queues the first returned.
///
/// Mutation (run): `fill_param_changes` emptying the set with
/// `out.queues.clear()` again (dropping every queue) → aborts on the second
/// gated block, freeing the queues the first took. Mutation (run): the
/// activation not reserving the output queues (`reserve_output_params`
/// removed) → no queue to open, so every point is dropped and the
/// assertions after the gate fail.
#[test]
fn param_output_burst_does_not_allocate() {
    let _lock = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    reset_probe();
    let mut inst = load_probe();
    assert!(
        inst.parameter_count() <= OUTPUT_QUEUES as usize,
        "the probe's parameter count sets the reservation; the burst assumes the floor"
    );
    let mut bufs = StereoBufs::new();
    let transport = TransportInfo::default();
    let ctx = ClapProcessContext {
        transport: Some(&transport),
        ..Default::default()
    };
    drive(&mut inst, &mut bufs, 32, &ctx).expect("warm-up blocks should succeed");

    const POINTS: u32 = 12;
    // Within the room: 192 events fit the output event list's reservation,
    // and 12 points fit a queue's.
    assert!(POINTS as usize <= tutti_plugin_types::ParameterQueue::chunk_room(MAX_FRAMES as usize));
    assert!(POINTS as usize > tutti_plugin_types::ParameterQueue::MIN_POINTS);
    set_param_output(OUTPUT_QUEUES, POINTS);

    assert_no_alloc::assert_no_alloc(|| {
        drive(&mut inst, &mut bufs, 64, &ctx).expect("process");
    });

    let queues = drive_and_read_param_output(&mut inst, &mut bufs);
    assert_eq!(
        queues.len(),
        OUTPUT_QUEUES as usize,
        "one queue per parameter"
    );
    for (i, &(id, points, last)) in queues.iter().enumerate() {
        assert_eq!(
            id,
            tutti_clap_test_plugin::rt_probe::PARAM_OUTPUT_FIRST_ID + i as u32
        );
        assert_eq!(points, POINTS as usize, "every point kept");
        assert_eq!(last, f64::from(POINTS - 1) / f64::from(POINTS));
    }
    assert_eq!(inst.output_param_points_dropped(), 0);
}

/// **Plugin parameter output past the reserved room is counted, not grown
/// into.** Two bursts, each inside the gate and each within the output event
/// list's own `PARAM_VALUE` room (16 queues × 20 points = 320 events), so
/// only the queues are under test:
///
/// - seventeen parameters with twelve points each: one more parameter than
///   the sixteen queues reserved. The first sixteen are kept whole; the last
///   one's twelve points are lost, and counted.
/// - four parameters with thirty points each: ten more than a queue's room
///   (`chunk_room(512)` = 20). Each queue keeps 20 points and ends on its
///   parameter's latest value (each point past the room replaced the last
///   one); 4 × 10 points a block are counted.
///
/// Mutation (run): `fill_param_changes` pushing without the `has_room`
/// check → aborts in the second burst (a queue's points grow). Mutation
/// (run): the overflow dropping the point instead of replacing the last one
/// → each queue ends on point 19's value, not point 29's, and the value
/// assertion fails. Mutation (run): `open_queue`'s refusal not counted → the
/// first burst's count is 0, not 12 a block.
#[test]
fn param_output_past_the_room_is_counted_not_grown() {
    let _lock = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    reset_probe();
    let mut inst = load_probe();
    let mut bufs = StereoBufs::new();
    let transport = TransportInfo::default();
    let ctx = ClapProcessContext {
        transport: Some(&transport),
        ..Default::default()
    };
    drive(&mut inst, &mut bufs, 32, &ctx).expect("warm-up blocks should succeed");
    let room = tutti_plugin_types::ParameterQueue::chunk_room(MAX_FRAMES as usize);
    assert_eq!(room, 20, "the burst sizes below assume a 512-frame room");

    // One parameter too many.
    const BLOCKS: u64 = 16;
    set_param_output(OUTPUT_QUEUES + 1, 12);
    assert_no_alloc::assert_no_alloc(|| {
        drive(&mut inst, &mut bufs, BLOCKS as usize, &ctx).expect("process");
    });
    assert_eq!(inst.output_param_points_dropped(), BLOCKS * 12);
    let queues = drive_and_read_param_output(&mut inst, &mut bufs);
    assert_eq!(
        queues.len(),
        OUTPUT_QUEUES as usize,
        "no queue past the reservation"
    );
    assert!(queues.iter().all(|&(_, points, _)| points == 12));
    let after_first = inst.output_param_points_dropped();
    assert_eq!(after_first, (BLOCKS + 1) * 12);

    // Ten points too many for each of four parameters.
    set_param_output(4, 30);
    assert_no_alloc::assert_no_alloc(|| {
        drive(&mut inst, &mut bufs, BLOCKS as usize, &ctx).expect("process");
    });
    assert_eq!(
        inst.output_param_points_dropped() - after_first,
        BLOCKS * 4 * 10
    );
    let queues = drive_and_read_param_output(&mut inst, &mut bufs);
    assert_eq!(queues.len(), 4);
    for &(_, points, last) in &queues {
        assert_eq!(points, room, "a queue fills to its room and no further");
        assert_eq!(
            last,
            29.0 / 30.0,
            "a full queue ends on its parameter's latest value"
        );
    }
}

/// **A plugin pushing more output events than the host reserved is refused,
/// not grown into.** The host reserves its output event list at activation
/// with room for every parameter's points (16 queues × `chunk_room(512)` =
/// 20: 320 parameter events), and 256 more for everything else. A plugin
/// pushing 40 parameters × 20 points = 800 parameter events a block, for ids
/// that are not its own (so none has a latest-value slot), is told `false`
/// for the last 480 of them each block, and the host counts each refusal
/// (`ClapActive::output_events_refused`) instead of growing its list in the
/// plugin's `try_push` on the audio thread.
///
/// Mutation (run): `output_events_try_push` without the parameter room check
/// → aborts (the list grows past its reservation inside the gate). Mutation (run):
/// the refusal not counted → the count assertion fails.
#[test]
fn output_events_past_the_room_are_refused_not_grown() {
    let _lock = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    reset_probe();
    let mut inst = load_probe();
    assert!(inst.parameter_count() <= OUTPUT_QUEUES as usize);
    let mut bufs = StereoBufs::new();
    let transport = TransportInfo::default();
    let ctx = ClapProcessContext {
        transport: Some(&transport),
        ..Default::default()
    };
    drive(&mut inst, &mut bufs, 32, &ctx).expect("warm-up blocks should succeed");
    let room = OUTPUT_QUEUES as u64
        * tutti_plugin_types::ParameterQueue::chunk_room(MAX_FRAMES as usize) as u64;
    assert_eq!(room, 320, "the parameter room");
    assert_eq!(inst.output_events_refused(), 0);

    const PARAMS: u32 = 40;
    const POINTS: u32 = 20;
    const BLOCKS: u64 = 16;
    set_param_output(PARAMS, POINTS);
    assert_no_alloc::assert_no_alloc(|| {
        drive(&mut inst, &mut bufs, BLOCKS as usize, &ctx).expect("process");
    });
    assert_eq!(
        inst.output_events_refused(),
        BLOCKS * (u64::from(PARAMS * POINTS) - room),
        "every push past the room is refused and counted"
    );
}

fn set_param_output_own(points: u32) {
    set_switch_u32(b"tutti_test_plugin_set_param_output_own\0", points);
}

/// **A parameter flood neither refuses a note-off nor loses a parameter's
/// final value.** The plugin floods its own four parameters with 200 points
/// each, every block (800 parameter events against a parameter room of 320),
/// and echoes a note-on and a note-off after the flood (the probe's note
/// mode). Under one shared room the note events came after the room was full
/// and were refused: a stuck note, since plugins rarely check `try_push`.
/// With separate rooms the note-off arrives every block, inside the gate,
/// and nothing is refused. The 480 parameter values past the room are
/// coalesced into a latest-value table, so every parameter still ends the
/// block on its last value, 199/200; the points that cost is counted.
///
/// Points lost a block: the list holds parameter 0's 200 points and 120 of
/// parameter 1's, which their 20-point queues reduce by 180 and 100; the
/// table keeps one value per parameter for the other 80 + 200 + 200 (477
/// overwritten), and parameter 1's latest replaces its queue's last point
/// (1 more). 758.
///
/// Mutation (run): one shared room for every event (`try_push` refusing any
/// push once the list holds `room + param_room`) → the note-off is refused
/// and the note-off assertion fails. Mutation (run): no latest-value table
/// (`coalesce_param` refusing every push) → the 480 values past the room
/// are refused (parameters 2 and 3 would end with no queue) and the refusal
/// assertion fails. Mutation (run): the parameter room check removed → the
/// flood grows the list inside the gate and aborts.
#[test]
fn a_param_flood_keeps_note_offs_and_final_values() {
    let _lock = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    reset_probe();
    set_switch_u32(b"tutti_test_plugin_set_render_mode\0", 4); // Notes: MIDI thru
    let mut inst = load_probe();
    let mut bufs = StereoBufs::new();
    let transport = TransportInfo::default();
    let notes = [
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000),
        MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0),
    ];
    let ctx = ClapProcessContext {
        midi: &notes,
        transport: Some(&transport),
        ..Default::default()
    };
    drive(&mut inst, &mut bufs, 32, &ctx).expect("warm-up blocks should succeed");
    let params = tutti_clap_test_plugin::params_state::probe_params();
    assert_eq!(
        params.len(),
        4,
        "the counts below assume the probe's four parameters"
    );
    let before = inst.output_param_points_dropped();

    const POINTS: u32 = 200;
    const BLOCKS: u64 = 16;
    set_param_output_own(POINTS);
    assert_no_alloc::assert_no_alloc(|| {
        drive(&mut inst, &mut bufs, BLOCKS as usize, &ctx).expect("process");
    });
    let (queues, note_offs) = drive_and_read_output(&mut inst, &mut bufs, &notes);
    set_switch_u32(b"tutti_test_plugin_set_render_mode\0", 0);

    assert_eq!(
        note_offs, 1,
        "the note-off must survive the parameter flood"
    );
    assert_eq!(
        inst.output_events_refused(),
        0,
        "the plugin's own ids are never refused"
    );
    assert_eq!(
        queues.len(),
        params.len(),
        "every flooded parameter has a queue"
    );
    for (&(id, _, last), param) in queues.iter().zip(params) {
        assert_eq!(id, param.id);
        assert_eq!(
            last,
            f64::from(POINTS - 1) / f64::from(POINTS),
            "parameter {id} must end on its final value"
        );
    }
    assert_eq!(
        inst.output_param_points_dropped() - before,
        (BLOCKS + 1) * 758
    );
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
/// CLAP marks `clap.log` `[thread-safe]`, which includes the audio thread.
/// Treating such a line like a main-thread one would mean `into_owned()`,
/// `eprintln!`, and `LogState::push` taking a `Mutex` that `drain_log` holds
/// across a `.collect()`. The first two are allocations; the third is a
/// priority inversion this gate cannot see, caught instead by the host refusing
/// to reach the lock from this thread.
///
/// `log_lines_dropped` is asserted to move, so a host that silently discarded
/// audio-thread logs would fail here rather than pass.
#[test]
fn audio_thread_logging_does_not_allocate() {
    let _lock = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    reset_probe();
    let mut inst = load_probe();
    let mut bufs = StereoBufs::new();
    let transport = TransportInfo::default();
    let ctx = ClapProcessContext {
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
    let ctx = ClapProcessContext {
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
// The host collected the caller's channel pointers into small-vector locals
// (16 pointers inline, then the heap), one per side, which spill to the heap at 17+ channels a side — every
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
    let ctx = ClapProcessContext {
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

/// **The control case.** Exactly 16 channels a side still fit the old host's
/// 16 inline pointers, so this passed even against the unfixed host.
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
/// heap-allocated two small-vector backing buffers on every block.
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
