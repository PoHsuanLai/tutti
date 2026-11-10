//! RT-safety and lifecycle conformance for the VST2 host.
//!
//! Two properties:
//!
//! 1. `update_transport` publishes the per-block transport snapshot without an
//!    allocation *or* a free inside the callback (`src/transport_cell.rs`,
//!    over `tutti-rt`'s `SeqWriter`).
//! 2. `set_sample_rate` / `set_block_size` bracket the change with an
//!    edge-triggered suspend/resume that tracks suspend state.
//!
//! The gate proves only that there is no *allocation*. It does not prove the
//! full RT-publishing property (no reader ever frees), and cannot: that hazard is a
//! race no sampling schedule exhausts. What stands in for it is structural —
//! the value is overwritten in place, so nothing is retired for a reader to
//! free. The concurrent-reader torture test lives in `transport_cell.rs`,
//! where it can see the private internals, and the proof is the sequence cell's loom
//! model (`tutti-rt/tests/seq_cell_loom.rs`).

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use assert_no_alloc::AllocDisabler;
use tutti_plugin_types::{MaxBlock, Prepare, SampleRate, Samples};
use tutti_ump::{MidiChannel, MidiGroup};
use tutti_vst2_host::{MidiEvent, TimeSignature, TransportInfo, Vst2Loaded, Vst2ProcessContext};
use tutti_vst2_test_plugin::ProcessCapture;

#[path = "support/active.rs"]
mod active;
#[path = "support/probe_path.rs"]
mod probe_path;

use active::Active;

// Not redundant with `src/lib.rs`: that declaration is `#[cfg(test)]`, which
// covers unit tests only, not integration-test binaries. Without this the
// `assert_no_alloc` gates below are silent no-ops that pass unconditionally.
#[global_allocator]
static A: AllocDisabler = AllocDisabler;

const SAMPLE_RATE: f64 = 48_000.0;
const BLOCK: usize = 64;

/// Serializes the whole drive→read sequence against the probe's one
/// process-global capture. The allocation tests need it for a second reason:
/// a concurrent test's allocations would be attributed to whichever thread is
/// inside the gate.
static PROBE_LOCK: Mutex<()> = Mutex::new(());

fn lock_probe() -> MutexGuard<'static, ()> {
    PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

fn load_probe() -> (Active, PathBuf) {
    load_probe_inner(false)
}

/// As [`load_probe`], with `effFlagsCanDoubleReplacing` declared so the host
/// takes the native f64 render path rather than the narrowing fallback.
fn load_probe_f64() -> (Active, PathBuf) {
    load_probe_inner(true)
}

fn load_probe_inner(declares_f64: bool) -> (Active, PathBuf) {
    let path = probe_path::probe_path().clone();
    reset_probe(&path);
    // SAFETY: callers hold `PROBE_LOCK`, so no other test thread is reading or
    // writing the environment concurrently.
    unsafe {
        if declares_f64 {
            std::env::set_var("TUTTI_VST2_PROBE_F64", "1");
        } else {
            std::env::remove_var("TUTTI_VST2_PROBE_F64");
        }
    }
    // Told 64-bit when the probe declares it: this file gates the native f64 path.
    let precision = if declares_f64 {
        active::Precision::Double
    } else {
        active::Precision::Single
    };
    let instance = Active::load_with_precision(&path, SAMPLE_RATE, BLOCK, precision)
        .unwrap_or_else(|e| panic!("host failed to load reference plugin at {path:?}: {e:?}"));
    // The AEffect is built; clearing keeps the variable from outliving this load.
    // SAFETY: as above.
    unsafe { std::env::remove_var("TUTTI_VST2_PROBE_F64") };
    (instance, path)
}

/// Re-open the image the host loaded and call one of the probe's exports.
/// Must go through `dlopen`: the linked rlib is a separate image with separate
/// statics, and only the cdylib's globals see the host's calls.
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
    let r = f(sym);

    // Leak the handle, deliberately. The switches these helpers touch are
    // `static`s inside the probe's image, and they only survive while that
    // image stays mapped — dropping `lib` decrements the refcount that keeps it
    // mapped. With no `Active` holding the probe open at that moment, the
    // write is discarded with the unload and the next load maps a fresh image
    // reading the default.
    //
    // Without the leak the lost write reads as flakiness, because it passes
    // whenever another test's instance happens to keep the image resident.
    std::mem::forget(lib);
    r
}

fn reset_probe(path: &PathBuf) {
    probe_call(path, b"tutti_vst2_probe_reset_capture\0", |sym| {
        // SAFETY: the probe exports this symbol with exactly this signature.
        let f: extern "C" fn() = unsafe { std::mem::transmute(*sym) };
        f();
    });
    probe_call(path, b"tutti_vst2_probe_reset_switches\0", |sym| {
        // SAFETY: the probe exports this symbol with exactly this signature.
        let f: extern "C" fn() = unsafe { std::mem::transmute(*sym) };
        f();
    });
}

fn read_capture(path: &PathBuf) -> ProcessCapture {
    let mut cap = ProcessCapture::empty();
    probe_call(path, b"tutti_vst2_probe_capture\0", |sym| {
        // SAFETY: the probe exports this symbol with exactly this signature.
        let f: unsafe extern "C" fn(*mut ProcessCapture) -> bool =
            unsafe { std::mem::transmute(*sym) };
        // SAFETY: `f` is the probe's capture export; it writes one
        // `ProcessCapture` through the pointer, and `cap` is a live local.
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

/// Render `iters` blocks of silence. Every buffer is on the stack, so any
/// allocation the gate observes came from the host, not this harness.
fn drive_silent(inst: &mut Active, iters: usize, transport: &TransportInfo) {
    let mut in_l = [0.0f32; BLOCK];
    let mut in_r = [0.0f32; BLOCK];
    let mut out_l = [0.0f32; BLOCK];
    let mut out_r = [0.0f32; BLOCK];
    let ctx = Vst2ProcessContext::new(SAMPLE_RATE).transport(transport);
    for _ in 0..iters {
        let ins: &[&[f32]] = &[&in_l[..], &in_r[..]];
        let outs: &mut [&mut [f32]] = &mut [&mut out_l[..], &mut out_r[..]];
        let _ = inst.process_f32(ins, outs, BLOCK, &ctx);
        // Touch the inputs so the optimiser cannot hoist the buffers out.
        in_l[0] = out_l[0] * 0.0;
        in_r[0] = out_r[0] * 0.0;
    }
}

/// The headline regression: a transport-carrying block must not allocate.
///
/// `ctx.transport` must stay `Some` on every iteration — the `Arc::new` sat
/// behind exactly that branch, so a `None` transport passes against the bug.
#[test]
fn process_f32_with_transport_does_not_allocate() {
    let _guard = lock_probe();
    let (mut inst, path) = load_probe();
    let transport = playing_transport();

    // Warm-up: grow the MIDI staging buffer and the pooled `midi_out` drain
    // past their inline capacities so steady-state calls are heap-free.
    drive_silent(&mut inst, 32, &transport);

    assert_no_alloc::assert_no_alloc(|| {
        drive_silent(&mut inst, 256, &transport);
    });

    // The gate is vacuous unless the plugin actually ran and read the
    // transport back — a host that skipped `process` trivially never allocates.
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

/// Render `iters` f64 blocks of silence, all buffers on the stack.
fn drive_silent_f64(inst: &mut Active, iters: usize, transport: &TransportInfo) {
    let mut in_l = [0.0f64; BLOCK];
    let mut in_r = [0.0f64; BLOCK];
    let mut out_l = [0.0f64; BLOCK];
    let mut out_r = [0.0f64; BLOCK];
    let ctx = Vst2ProcessContext::new(SAMPLE_RATE).transport(transport);
    for _ in 0..iters {
        let ins: &[&[f64]] = &[&in_l[..], &in_r[..]];
        let outs: &mut [&mut [f64]] = &mut [&mut out_l[..], &mut out_r[..]];
        let _ = inst.process_f64(ins, outs, BLOCK, &ctx);
        // Touch the inputs so the optimiser cannot hoist the buffers out.
        in_l[0] = out_l[0] * 0.0;
        in_r[0] = out_r[0] * 0.0;
    }
}

/// The f64 render path must be allocation-free too.
///
/// `RenderScratch` grew a second set of channel buffers and pointer tables to
/// reach `processReplacingF64`; sizing them lazily on the first f64 block
/// would put a `Vec` growth inside the callback. They are allocated in
/// `RenderScratch::new` instead, and this is what holds that.
#[test]
fn process_f64_does_not_allocate() {
    let _guard = lock_probe();
    let (mut inst, path) = load_probe_f64();
    assert!(
        inst.metadata().supports_f64,
        "the probe must declare f64 here, or this gates the narrowing \
         fallback rather than the native f64 path"
    );
    let transport = playing_transport();

    drive_silent_f64(&mut inst, 32, &transport);

    assert_no_alloc::assert_no_alloc(|| {
        drive_silent_f64(&mut inst, 256, &transport);
    });

    let cap = read_capture(&path);
    assert!(cap.valid, "probe observed no render");
    assert!(
        cap.process_calls >= 256,
        "expected at least the gated renders, saw {}",
        cap.process_calls
    );
    assert_eq!(
        cap.entry,
        tutti_vst2_test_plugin::ProcessEntry::ReplacingF64,
        "the gate covered the f32 slot, not the f64 one it exists for"
    );
}

/// Same gate with MIDI in flight, so the staging/drain pools are exercised
/// alongside the transport publish.
#[test]
fn process_f32_with_midi_does_not_allocate() {
    let _guard = lock_probe();
    let (mut inst, path) = load_probe();
    let transport = playing_transport();

    let mut out_l = [0.0f32; BLOCK];
    let mut out_r = [0.0f32; BLOCK];

    // Warm-up: a note pair plus silence, so first-call lazy allocations in
    // both the plugin and the host's pools settle before the gate.
    {
        let ins: &[&[f32]] = &[];
        let outs: &mut [&mut [f32]] = &mut [&mut out_l[..], &mut out_r[..]];
        let warm = [
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x4000),
            MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0),
        ];
        let ctx = Vst2ProcessContext::new(SAMPLE_RATE)
            .midi(&warm)
            .transport(&transport);
        let _ = inst.process_f32(ins, outs, BLOCK, &ctx);
    }
    drive_silent(&mut inst, 32, &transport);

    let on_event = [MidiEvent::note_on(
        MidiGroup::FIRST,
        MidiChannel::FIRST,
        60,
        0x4000,
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
            let ins: &[&[f32]] = &[];
            let outs: &mut [&mut [f32]] = &mut [&mut out_l[..], &mut out_r[..]];
            let ctx = Vst2ProcessContext::new(SAMPLE_RATE)
                .midi(events)
                .transport(&transport);
            let _ = inst.process_f32(ins, outs, BLOCK, &ctx);
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

/// A rejoin suspends the plugin once, an activation resumes it once, and a
/// plugin that is only loaded is neither.
///
/// The probe's counters say what crossed the FFI seam, not what the host's own
/// flag claims. Before the activation split a reconfigure of an
/// already-suspended plugin dispatched a second `effMainsChanged(0)` and then
/// silently resumed on the way out; now a suspended plugin is a `Vst2Loaded`
/// with no halves to reconfigure.
///
/// Mutation (run): `PluginInstance::close` suspending without checking `resumed` ->
/// the dropped plugin suspends a second time -> fails. Mutation (run):
/// `activate` not setting `resumed` -> the rejoin does not suspend -> fails.
#[test]
fn a_rejoin_suspends_once_and_an_activation_resumes_once() {
    let _guard = lock_probe();
    let (inst, path) = load_probe();

    // An activation ends resumed: one resume, no suspend.
    let base = read_capture(&path);
    assert_eq!(base.resume_count, 1, "activate must resume exactly once");
    assert_eq!(base.suspend_count, 0, "activate should not leave a suspend");
    assert!(inst.is_active());

    let loaded = inst.deactivate();
    assert!(!loaded.is_active());
    assert_eq!(read_capture(&path).suspend_count, 1, "one suspend");
    assert_eq!(read_capture(&path).resume_count, 1);

    // Dropping a rejoined plugin is not a second suspend.
    drop(loaded);
    assert_eq!(
        read_capture(&path).suspend_count,
        1,
        "the drop of a suspended plugin reached the plugin"
    );

    // A second activation, at another rate, is a single, real transition.
    let loaded =
        Vst2Loaded::load(&main_thread(), probe_path::probe_path()).expect("the probe loads");
    assert_eq!(
        read_capture(&path).resume_count,
        1,
        "a load does not resume"
    );
    let inst = Active::activate(loaded, active::prepare_for(96_000.0, BLOCK, false))
        .expect("the probe activates");
    let cap = read_capture(&path);
    assert_eq!(cap.resume_count, 2);
    assert_eq!(
        cap.sample_rate, 96_000.0,
        "the activation's rate took effect"
    );
    assert!(inst.is_active());
}

/// A rate or block that is none cannot be built into a `Prepare`, and a valid
/// activation after the refused ones is the first resume. (The refusal reaching
/// the plugin is tested through the server loaders, which take a bare rate.)
///
/// Mutation (run): the finite and positive check removed from `Prepare::new` ->
/// the zero and NaN rates build a `Prepare` -> the `is_none` fails.
#[test]
fn a_prepare_that_is_no_rate_cannot_be_built() {
    let _guard = lock_probe();
    let path = probe_path::probe_path().clone();
    reset_probe(&path);
    let loaded = Vst2Loaded::load(&main_thread(), &path).expect("the probe loads");

    for (rate, block) in [
        (0.0, BLOCK),
        (f64::NAN, BLOCK),
        (-48_000.0, BLOCK),
        (48_000.0, 0),
    ] {
        let prepare =
            MaxBlock::new(Samples(block)).and_then(|block| Prepare::new(SampleRate(rate), block));
        assert!(
            prepare.is_none(),
            "{rate} Hz in {block} frames must be refused"
        );
    }

    let inst = Active::activate(loaded, active::prepare_for(SAMPLE_RATE, BLOCK, false))
        .expect("the same plugin activates at a valid rate");
    assert_eq!(read_capture(&path).resume_count, 1);
    assert!(inst.is_active());
}

/// Reconfiguring a *resumed* plugin must leave it resumed, with exactly one
/// balanced suspend/resume pair per call — the complement of the
/// already-suspended case above.
#[test]
fn reconfigure_while_resumed_issues_one_balanced_pair() {
    let _guard = lock_probe();
    let (mut inst, path) = load_probe();

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
    assert!(inst.is_active());
}

/// A reconfigure must leave the plugin in a state where it still renders —
/// the end-to-end consequence of an unbalanced suspend, which leaves a
/// powered-down plugin that the host happily keeps feeding blocks.
///
/// The `effStartProcess` / `effStopProcess` dispatch in `tutti-vst2-ffi/src/host.rs`
/// is *not* covered: `ProcessCapture` has no counter for those opcodes.
#[test]
fn plugin_still_renders_after_repeated_reconfigure() {
    let _guard = lock_probe();
    let (mut inst, path) = load_probe();

    inst.set_sample_rate(96_000.0);
    inst.set_sample_rate(SAMPLE_RATE);
    assert!(inst.is_active());

    let transport = playing_transport();
    drive_silent(&mut inst, 4, &transport);

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

/// This process's main thread. Each test is its own process under nextest, so
/// the claim cannot be refused.
fn main_thread() -> tutti_plugin_types::MainThread {
    tutti_plugin_types::MainThread::claim().expect("one test, one process")
}
