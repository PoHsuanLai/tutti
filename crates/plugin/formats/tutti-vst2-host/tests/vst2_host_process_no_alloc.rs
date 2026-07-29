//! RT-safety and lifecycle conformance for the VST2 host.
//!
//! Two properties, both regression tests for confirmed bugs:
//!
//! 1. **`process_f32` must not allocate on the audio thread.**
//!    `update_transport` used to run `time_info.store(Arc::new(Some(next)))`
//!    every block — a heap allocation *and* a free of the retired snapshot,
//!    inside the callback, on the primary path. See
//!    `src/transport_cell.rs` for the seqlock that replaced it.
//!
//! 2. **The suspend/resume state machine must be idempotent.**
//!    `set_sample_rate` / `set_block_size` used to run an unconditional
//!    `suspend(); set(); resume()` with nothing tracking whether the plugin
//!    was already suspended, so calling either twice double-suspended it.
//!
//! # What the no-alloc gate can and cannot prove
//!
//! The allocation tests here prove the *allocation* is gone: `AllocDisabler`
//! panics on any `malloc` inside the gated scope, and the mutation record in
//! the task report shows they fail when the seqlock is reverted.
//!
//! They do **not** prove the RT-publishing property CLAUDE.md describes, and
//! this file does not claim to. That policy is explicit that a no-alloc test
//! *cannot* pin it — the hazard is a race between a reader holding a retired
//! value and a writer freeing it, and no sampling schedule exhausts a race.
//! What replaces that proof here is structural, not statistical: the value is
//! overwritten in place, so there is no retired allocation for anyone to free.
//! The concurrent-reader torture test lives next to the implementation, in
//! `transport_cell.rs`'s unit tests, where it can see the private internals.
//!
//! # Why this file loads the in-repo probe
//!
//! It previously pointed at `/Library/Audio/Plug-Ins/VST/TAL-NoiseMaker.vst`
//! — a macOS path, on a Linux host — *and* was `#[ignore]`d, *and* guarded by
//! a `load_or_skip()` returning `None`. Three independent reasons it could
//! never fail. It now loads the reference probe, which is built by a
//! dev-dependency edge in this same `cargo test` invocation, and panics if it
//! is missing (see `tests/support/probe_path.rs`).

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use assert_no_alloc::AllocDisabler;
use tutti_vst2_host::{
    MidiEvent, ProcessContext, RenderScratch, TimeSignature, TransportInfo, Vst2Instance,
};
use tutti_vst2_test_plugin::ProcessCapture;

#[path = "support/probe_path.rs"]
mod probe_path;

// The `assert_no_alloc` checks below are inert unless `AllocDisabler` is the
// active global allocator for THIS test binary — without it the gate is a
// silent no-op that passes unconditionally. The `#[cfg(test)]` declaration in
// `src/lib.rs` applies to unit tests only, not to integration-test binaries,
// so it must be declared here. Verified by mutation: commenting this out makes
// the reverted-seqlock run pass, which is the failure mode it guards against.
#[global_allocator]
static A: AllocDisabler = AllocDisabler;

const SAMPLE_RATE: f64 = 48_000.0;
const BLOCK: usize = 64;

/// The probe writes into one process-global capture inside a single loaded
/// image and `cargo test` runs test fns on parallel threads, so the whole
/// drive→read sequence is serialized. The allocation tests need this for a
/// second reason: a concurrent test's allocations would otherwise be attributed
/// to whichever thread is inside the gate.
static PROBE_LOCK: Mutex<()> = Mutex::new(());

fn lock_probe() -> MutexGuard<'static, ()> {
    PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

fn load_probe() -> (Vst2Instance, RenderScratch, PathBuf) {
    let path = probe_path::probe_path().clone();
    reset_probe(&path);
    let instance = Vst2Instance::load(&path, SAMPLE_RATE, BLOCK)
        .unwrap_or_else(|e| panic!("host failed to load reference plugin at {path:?}: {e:?}"));
    let meta = instance.metadata().clone();
    let scratch = RenderScratch::new(meta.num_inputs, meta.num_outputs, BLOCK);
    (instance, scratch, path)
}

/// Re-open the image the host loaded and call one of the probe's exports.
/// The linked rlib is a separate image with separate statics; only the
/// cdylib's globals see the host's calls, and `dlopen` on the same path
/// returns the already-loaded image. Mirrors `vst2_probe_smoke.rs`.
fn probe_call<F, R>(path: &PathBuf, symbol: &[u8], f: F) -> R
where
    F: FnOnce(libloading::Symbol<'_, *mut std::ffi::c_void>) -> R,
{
    // SAFETY: the path is the cdylib this crate's dev-dependency built.
    let lib = unsafe { libloading::Library::new(path) }
        .unwrap_or_else(|e| panic!("re-open reference plugin at {path:?}: {e}"));
    // SAFETY: the symbol names are the probe's `#[no_mangle]` exports.
    let sym: libloading::Symbol<*mut std::ffi::c_void> =
        unsafe { lib.get(symbol) }.unwrap_or_else(|e| {
            panic!(
                "probe missing symbol {}: {e}",
                String::from_utf8_lossy(symbol)
            )
        });
    f(sym)
}

fn reset_probe(path: &PathBuf) {
    probe_call(path, b"tutti_vst2_probe_reset_capture\0", |sym| {
        let f: extern "C" fn() = unsafe { std::mem::transmute(*sym) };
        f();
    });
    probe_call(path, b"tutti_vst2_probe_reset_switches\0", |sym| {
        let f: extern "C" fn() = unsafe { std::mem::transmute(*sym) };
        f();
    });
}

fn read_capture(path: &PathBuf) -> ProcessCapture {
    let mut cap = ProcessCapture::empty();
    probe_call(path, b"tutti_vst2_probe_capture\0", |sym| {
        let f: unsafe extern "C" fn(*mut ProcessCapture) -> bool =
            unsafe { std::mem::transmute(*sym) };
        unsafe { f(&mut cap) }
    });
    cap
}

fn playing_transport() -> TransportInfo {
    TransportInfo::default()
        .with_playing(true)
        .with_tempo(120.0)
        .with_time_signature(TimeSignature::default())
}

/// Render `iters` blocks of silence. Every buffer lives on the stack, so any
/// allocation the gate observes came from the host, not from this harness.
fn drive_silent(
    inst: &mut Vst2Instance,
    scratch: &mut RenderScratch,
    iters: usize,
    transport: &TransportInfo,
) {
    let mut in_l = [0.0f32; BLOCK];
    let mut in_r = [0.0f32; BLOCK];
    let mut out_l = [0.0f32; BLOCK];
    let mut out_r = [0.0f32; BLOCK];
    let ctx = ProcessContext::new(SAMPLE_RATE).transport(transport);
    for _ in 0..iters {
        let ins: &[&[f32]] = &[&in_l[..], &in_r[..]];
        let outs: &mut [&mut [f32]] = &mut [&mut out_l[..], &mut out_r[..]];
        let _ = inst.process_f32(ins, outs, BLOCK, &ctx, scratch);
        // Touch the inputs so the optimiser cannot hoist the buffers out.
        in_l[0] = out_l[0] * 0.0;
        in_r[0] = out_r[0] * 0.0;
    }
}

/// The headline regression: a transport-carrying block must not allocate.
///
/// `ctx.transport` is `Some` on every iteration, which is what drives
/// `update_transport` down the path that used to call `Arc::new`. A version of
/// this test that left the transport `None` would pass against the buggy code,
/// because the allocation sat behind exactly that branch.
#[test]
fn process_f32_with_transport_does_not_allocate() {
    let _guard = lock_probe();
    let (mut inst, mut scratch, path) = load_probe();
    let transport = playing_transport();

    // Warm-up: grow the MIDI staging buffer and the pooled `midi_out` drain
    // past their inline capacities so steady-state calls are heap-free.
    drive_silent(&mut inst, &mut scratch, 32, &transport);

    assert_no_alloc::assert_no_alloc(|| {
        drive_silent(&mut inst, &mut scratch, 256, &transport);
    });

    // The gate only means something if the plugin actually ran and actually
    // read the transport back. Without this, a host that silently skipped
    // `process` would trivially "not allocate".
    let cap = read_capture(&path);
    assert!(cap.valid, "probe observed no render");
    assert!(
        cap.process_calls >= 256,
        "expected at least the gated renders, saw {}",
        cap.process_calls
    );
    assert!(
        cap.time_info_present,
        "probe's audioMasterGetTime returned nothing — the re-entrant read \
         path this test exists to cover was never exercised"
    );
    assert_eq!(cap.time_tempo, 120.0, "host served the wrong tempo");
}

/// Same gate with MIDI in flight, so the staging/drain pools are exercised
/// alongside the transport publish rather than only in isolation.
#[test]
fn process_f32_with_midi_does_not_allocate() {
    let _guard = lock_probe();
    let (mut inst, mut scratch, path) = load_probe();
    let transport = playing_transport();

    let mut out_l = [0.0f32; BLOCK];
    let mut out_r = [0.0f32; BLOCK];

    // Warm-up: a note pair plus silence, so first-call lazy allocations in
    // both the plugin and the host's pools settle before the gate.
    {
        let ins: &[&[f32]] = &[];
        let outs: &mut [&mut [f32]] = &mut [&mut out_l[..], &mut out_r[..]];
        let warm = [
            MidiEvent::note_on(0, 0, 60, 0x4000),
            MidiEvent::note_off(0, 0, 60, 0),
        ];
        let ctx = ProcessContext::new(SAMPLE_RATE)
            .midi(&warm)
            .transport(&transport);
        let _ = inst.process_f32(ins, outs, BLOCK, &ctx, &mut scratch);
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
            let ins: &[&[f32]] = &[];
            let outs: &mut [&mut [f32]] = &mut [&mut out_l[..], &mut out_r[..]];
            let ctx = ProcessContext::new(SAMPLE_RATE)
                .midi(events)
                .transport(&transport);
            let _ = inst.process_f32(ins, outs, BLOCK, &ctx, &mut scratch);
        }
    });

    let cap = read_capture(&path);
    assert!(cap.valid, "probe observed no render");
    assert!(
        cap.total_event_count >= 8,
        "expected the gated MIDI to reach the plugin, saw {}",
        cap.total_event_count
    );
}

/// Reconfiguring an **already-suspended** plugin must not suspend it again,
/// and must leave it suspended.
///
/// This is the exact shape of the bug, and the reason a "call `set_sample_rate`
/// twice" test is not enough: the pre-fix code ran an unconditional
/// `suspend(); set(); resume()`, so back-to-back reconfigures of a *resumed*
/// plugin still produced one balanced pair each and looked correct. The defect
/// only becomes observable when the plugin is already suspended on entry —
/// then the old code dispatched a second `effMainsChanged(0)` to a plugin that
/// had already powered down and, worse, silently *resumed* it on the way out.
/// VST 2.4 does not document `effMainsChanged` as idempotent, and real plugins
/// reallocate on every `resume(1)`.
///
/// Counters are read from the probe, so this asserts what actually crossed the
/// FFI seam rather than trusting the host's own flag.
#[test]
fn reconfigure_while_suspended_does_not_double_suspend_or_silently_resume() {
    let _guard = lock_probe();
    let (mut inst, _scratch, path) = load_probe();

    // `load` ends resumed: one resume, no suspend.
    let base = read_capture(&path);
    assert_eq!(base.resume_count, 1, "load must resume exactly once");
    assert_eq!(base.suspend_count, 0, "load should not leave a suspend");
    assert!(inst.is_resumed());

    // Explicitly suspend, then reconfigure while down.
    assert!(inst.suspend(), "first suspend must dispatch");
    assert!(!inst.is_resumed());
    assert_eq!(read_capture(&path).suspend_count, 1);

    inst.set_sample_rate(96_000.0);

    let after = read_capture(&path);
    assert_eq!(
        after.suspend_count, 1,
        "reconfiguring an already-suspended plugin must not suspend it again"
    );
    assert_eq!(
        after.resume_count, 1,
        "reconfiguring a suspended plugin must not silently resume it"
    );
    assert!(
        !inst.is_resumed(),
        "the plugin was suspended on entry and must stay suspended"
    );
    // The reconfigure itself must still have taken effect.
    assert_eq!(after.sample_rate, 96_000.0);

    // Resuming afterwards is a single, real transition.
    assert!(inst.resume(), "resume must dispatch when suspended");
    assert_eq!(read_capture(&path).resume_count, 2);
    assert!(inst.is_resumed());
}

/// `suspend` / `resume` must be idempotent: a redundant call dispatches
/// nothing across the FFI seam.
///
/// Nothing in the crate tracked suspend state before this fix — there was no
/// `resumed` flag anywhere — so every call was unconditionally dispatched.
#[test]
fn repeated_suspend_and_resume_are_idempotent() {
    let _guard = lock_probe();
    let (mut inst, _scratch, path) = load_probe();

    // Already resumed from load, so this must be a no-op.
    assert!(
        !inst.resume(),
        "resume on a resumed plugin must not dispatch"
    );
    assert_eq!(read_capture(&path).resume_count, 1);

    assert!(inst.suspend());
    assert!(
        !inst.suspend(),
        "suspend on a suspended plugin must not dispatch"
    );
    assert_eq!(
        read_capture(&path).suspend_count,
        1,
        "the redundant suspend reached the plugin"
    );

    assert!(inst.resume());
    assert!(!inst.resume());
    assert_eq!(
        read_capture(&path).resume_count,
        2,
        "the redundant resume reached the plugin"
    );
}

/// The same bracket on `set_block_size`, which shares the implementation.
///
/// It has no in-tree caller today (verified by ripgrep across both
/// workspaces), so this test is the only thing holding its lifecycle correct —
/// which is precisely why it is here rather than omitted as dead weight.
#[test]
fn set_block_size_while_suspended_does_not_double_suspend() {
    let _guard = lock_probe();
    let (mut inst, _scratch, path) = load_probe();

    assert!(inst.suspend());
    inst.set_block_size(512);

    let cap = read_capture(&path);
    assert_eq!(cap.suspend_count, 1, "must not re-suspend");
    assert_eq!(cap.resume_count, 1, "must not silently resume");
    assert!(!inst.is_resumed());
    assert_eq!(cap.max_block_size, 512, "the reconfigure must take effect");
}

/// Reconfiguring a *resumed* plugin must leave it resumed, with exactly one
/// balanced suspend/resume pair per call — the complement of the
/// already-suspended case above.
#[test]
fn reconfigure_while_resumed_issues_one_balanced_pair() {
    let _guard = lock_probe();
    let (mut inst, _scratch, path) = load_probe();

    inst.set_sample_rate(96_000.0);
    inst.set_sample_rate(44_100.0);

    let cap = read_capture(&path);
    assert_eq!(cap.suspend_count, 2, "one suspend per reconfigure");
    assert_eq!(
        cap.resume_count, 3,
        "load's resume plus one per reconfigure"
    );
    assert_eq!(
        cap.resume_count,
        cap.suspend_count + 1,
        "resume/suspend must alternate, with load's initial resume unmatched"
    );
    assert_eq!(cap.sample_rate, 44_100.0);
    assert!(inst.is_resumed());
}

/// A reconfigure must leave the plugin in a state where it still renders.
///
/// This is the end-to-end consequence of getting the lifecycle wrong: an
/// unbalanced suspend, or a `resume` the plugin refused because it was never
/// suspended, leaves a plugin that is powered down while the host happily
/// feeds it blocks. Asserting the audio still comes out is what makes the
/// state machine's correctness observable without reaching into the probe for
/// opcode-level counters it does not yet record.
///
/// The `effStartProcess` / `effStopProcess` dispatch added alongside this
/// (see `vst-tutti/src/host.rs`) is *not* asserted here: the probe's
/// `ProcessCapture` has no counter for those opcodes, and that file belongs to
/// another agent. Flagged in the task report as the one unverified piece.
#[test]
fn plugin_still_renders_after_repeated_reconfigure() {
    let _guard = lock_probe();
    let (mut inst, mut scratch, path) = load_probe();

    inst.set_sample_rate(96_000.0);
    inst.set_sample_rate(SAMPLE_RATE);
    assert!(inst.is_resumed());

    let transport = playing_transport();
    drive_silent(&mut inst, &mut scratch, 4, &transport);

    let cap = read_capture(&path);
    assert!(
        cap.valid && cap.process_calls >= 4,
        "plugin stopped rendering after a reconfigure — the suspend/resume \
         bracket left it powered down (process_calls = {})",
        cap.process_calls
    );
    assert_eq!(
        cap.sample_rate, SAMPLE_RATE as f32,
        "the last configured rate must be the one in effect"
    );
}
