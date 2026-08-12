//! `process_f64` must reach `processReplacingF64`, not narrow through f32.
//!
//! The host built a `vst::buffer::AudioBuffer<f32>` for both entry points, so
//! an f64 block was staged down, rendered at f32, and widened back — while
//! `Features::F64_AUDIO` told the negotiation layer the chain was 64-bit.
//! The vendor's `processReplacingF64` call had zero callers in the tree.
//!
//! Two independent witnesses, because either alone is satisfiable by the bug:
//!
//! - **Which slot ran.** `ProcessCapture::entry` records the AEffect member
//!   the probe was entered through. A structural check only.
//! - **What survived.** A sample needing more than f32's 24-bit mantissa
//!   comes back bit-for-bit. This is what a caller actually loses, and it
//!   fails on a host that calls the right slot with narrowed staging.
//!
//! `effSetProcessPrecision` (77) is covered here too: it is the opcode that
//! tells a dual-precision plugin which width to configure for, and it was
//! never sent.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use tutti_vst2_host::{ProcessContext, RenderScratch, Vst2Instance};
use tutti_vst2_test_plugin::{ProcessCapture, ProcessEntry};

#[path = "support/probe_path.rs"]
mod probe_path;

const SAMPLE_RATE: f64 = 48_000.0;
const BLOCK: usize = 64;

/// A value whose f32 round trip is lossy by enough to survive the probe's
/// oracle.
///
/// The oracle is `out = in + channel_tag(ch)`, and the tags run 1.0, 101.0,
/// 201.0 … — so the narrowing has to be visible *after* adding a number two
/// orders of magnitude larger. `1.0 + f64::EPSILON` does not qualify and was
/// the first choice here: its f32 error is one f64 ULP at 1.0, which the
/// addition rounds straight back off, making the fixture pass against the very
/// bug it targets.
///
/// `1/3` has an f32 error near `1e-8` — about six orders of magnitude above
/// the ULP of 701.0 — so it survives every tag the probe can produce.
/// Denormals were the other candidate and are worse here: they invite a
/// plugin's flush-to-zero to answer instead of the host's cast, and they
/// vanish entirely under the tag.
const NEEDS_F64: f64 = 1.0 / 3.0;

/// Serializes the process-global probe environment, switches and capture
/// across the whole set→load→drive→read sequence.
static PROBE_LOCK: Mutex<()> = Mutex::new(());

fn lock_probe() -> MutexGuard<'static, ()> {
    PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// Every `TUTTI_VST2_PROBE_*` key this file sets. A leaked variable reshapes
/// every plugin loaded afterwards in this binary.
const PROBE_ENV_KEYS: &[&str] = &["TUTTI_VST2_PROBE_F64"];

fn clear_probe_env() {
    for key in PROBE_ENV_KEYS {
        // SAFETY: callers hold `PROBE_LOCK`, so no other test thread is
        // reading or writing the environment concurrently.
        unsafe { std::env::remove_var(key) };
    }
}

fn probe_call<F, R>(path: &Path, symbol: &[u8], f: F) -> R
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
    // mapped. With no `Vst2Instance` holding the probe open at that moment, the
    // write is discarded with the unload and the next load maps a fresh image
    // reading the default.
    //
    // Measured on this bug in `vst2_latency.rs`: 2 of 6 runs failed without
    // this, 0 of 6 with it. It reads as flakiness because it passes whenever
    // another test's instance happens to keep the image resident.
    std::mem::forget(lib);
    r
}

fn reset_probe(path: &Path) {
    probe_call(path, b"tutti_vst2_probe_reset_capture\0", |sym| {
        let f: extern "C" fn() = unsafe { std::mem::transmute(*sym) };
        f();
    });
    probe_call(path, b"tutti_vst2_probe_reset_switches\0", |sym| {
        let f: extern "C" fn() = unsafe { std::mem::transmute(*sym) };
        f();
    });
}

fn read_capture(path: &Path) -> ProcessCapture {
    let mut cap = ProcessCapture::empty();
    probe_call(path, b"tutti_vst2_probe_capture\0", |sym| {
        let f: unsafe extern "C" fn(*mut ProcessCapture) -> bool =
            unsafe { std::mem::transmute(*sym) };
        unsafe { f(&mut cap) }
    });
    cap
}

/// Load the probe, with `effFlagsCanDoubleReplacing` set or clear.
fn load_probe(declares_f64: bool) -> (Vst2Instance, RenderScratch, PathBuf) {
    let path = probe_path::probe_path().clone();
    reset_probe(&path);
    clear_probe_env();
    if declares_f64 {
        // SAFETY: the caller holds `PROBE_LOCK`.
        unsafe { std::env::set_var("TUTTI_VST2_PROBE_F64", "1") };
    }
    let instance = Vst2Instance::load(&path, SAMPLE_RATE, BLOCK)
        .unwrap_or_else(|e| panic!("host failed to load reference plugin at {path:?}: {e:?}"));
    // The AEffect is built; clearing now keeps the variable from outliving
    // this load.
    clear_probe_env();
    let meta = instance.metadata().clone();
    let scratch = RenderScratch::new(meta.num_inputs, meta.num_outputs, BLOCK);
    (instance, scratch, path)
}

/// Render one f64 block of `input_value` on every channel, returning the
/// per-channel output.
fn render_f64(
    instance: &mut Vst2Instance,
    scratch: &mut RenderScratch,
    input_value: f64,
) -> Vec<Vec<f64>> {
    let meta = instance.metadata().clone();
    let inputs = meta.num_inputs.count() as usize;
    let outputs = meta.num_outputs.count() as usize;

    let input_data = vec![vec![input_value; BLOCK]; inputs];
    let mut output_data = vec![vec![0.0f64; BLOCK]; outputs];

    let input_slices: Vec<&[f64]> = input_data.iter().map(|v| v.as_slice()).collect();
    let mut output_slices: Vec<&mut [f64]> =
        output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

    let ctx = ProcessContext::new(SAMPLE_RATE);
    instance.process_f64(&input_slices, &mut output_slices, BLOCK, &ctx, scratch);

    output_data
}

/// A plugin declaring `effFlagsCanDoubleReplacing` must be entered through
/// `processReplacingF64`.
///
/// Pre-fix this reads `Replacing`: `process_block` built an
/// `AudioBuffer<f32>` for both public entry points.
#[test]
fn f64_capable_plugin_is_entered_through_the_f64_slot() {
    let _guard = lock_probe();
    let (mut instance, mut scratch, path) = load_probe(true);
    assert!(
        instance.metadata().supports_f64,
        "the probe was asked to declare effFlagsCanDoubleReplacing but the \
         host did not read it back — the rest of this test would be vacuous"
    );

    let _ = render_f64(&mut instance, &mut scratch, 0.0);

    let cap = read_capture(&path);
    assert!(cap.valid, "probe observed no render");
    assert_eq!(
        cap.entry,
        ProcessEntry::ReplacingF64,
        "process_f64 rendered through {:?}; an f64-capable plugin must be \
         entered through processReplacingF64",
        cap.entry
    );
}

/// The consequence a caller can measure: a sample needing more than f32's
/// mantissa must survive the round trip exactly.
///
/// The probe's oracle is `out = in + channel_tag(ch)`, and every tag is a
/// small integer that is exact at both widths, so the only thing that can
/// perturb the result is the host's own staging.
#[test]
fn f64_render_preserves_precision_f32_cannot_carry() {
    let _guard = lock_probe();
    let (mut instance, mut scratch, path) = load_probe(true);
    let meta = instance.metadata().clone();

    // Guard the fixture, per channel, against the tag rounding the difference
    // back off. Checking only the bare round trip is not enough — that is
    // exactly how the first version of this test passed against the bug.
    for ch in 0..meta.num_outputs.count() as usize {
        let tag = tutti_vst2_test_plugin::channel_tag(ch) as f64;
        assert_ne!(
            NEEDS_F64 + tag,
            NEEDS_F64 as f32 as f64 + tag,
            "fixture is vacuous on channel {ch}: adding the tag {tag} rounds \
             the f32 narrowing away, so both paths would produce the same \
             sample"
        );
    }

    let out = render_f64(&mut instance, &mut scratch, NEEDS_F64);

    let cap = read_capture(&path);
    assert!(cap.valid, "probe observed no render");

    for (ch, samples) in out.iter().enumerate() {
        let tag = tutti_vst2_test_plugin::channel_tag(ch) as f64;
        let expected = NEEDS_F64 + tag;
        for (i, &s) in samples.iter().enumerate() {
            assert_eq!(
                s, expected,
                "channel {ch} sample {i}: the host narrowed the block through \
                 f32 (expected {expected:?}, got {s:?})"
            );
        }
    }
}

/// The negative control for both tests above: a plugin that does NOT declare
/// `effFlagsCanDoubleReplacing` has no f64 entry point, so the host must fall
/// back to the f32 slot rather than call a slot the plugin never installed.
///
/// Without this, "always call processReplacingF64" would pass the two tests
/// above while rendering silence here — the vendor's `process_f64` zeroes the
/// outputs when `can_double_replacing` is clear.
#[test]
fn plugin_without_f64_support_falls_back_to_the_f32_slot() {
    let _guard = lock_probe();
    let (mut instance, mut scratch, path) = load_probe(false);
    assert!(
        !instance.metadata().supports_f64,
        "the default probe must not declare effFlagsCanDoubleReplacing"
    );

    let out = render_f64(&mut instance, &mut scratch, 0.0);

    let cap = read_capture(&path);
    assert!(cap.valid, "probe observed no render");
    assert_eq!(
        cap.entry,
        ProcessEntry::Replacing,
        "a plugin without processReplacingF64 must be entered through the f32 \
         slot, not one it never installed"
    );

    // And the fallback must still deliver audio rather than the silence the
    // vendor returns for a missing f64 slot.
    for (ch, samples) in out.iter().enumerate() {
        let expected = tutti_vst2_test_plugin::channel_tag(ch) as f64;
        assert_eq!(
            samples[0], expected,
            "channel {ch}: the f32 fallback rendered silence instead of the \
             probe's channel tag"
        );
    }
}

/// `effSetProcessPrecision` must be dispatched at load, carrying the width the
/// plugin declared. It was never sent, so a plugin that configures its
/// internal precision on this opcode stayed wherever it defaulted.
#[test]
fn set_process_precision_is_dispatched_with_the_declared_width() {
    let _guard = lock_probe();

    let (instance, _scratch, path) = load_probe(true);
    let cap = read_capture(&path);
    assert_eq!(
        cap.set_precision_count, 1,
        "effSetProcessPrecision was dispatched {} times at load; expected \
         exactly one",
        cap.set_precision_count
    );
    assert_eq!(
        cap.set_precision_value, 1,
        "the plugin declared effFlagsCanDoubleReplacing, so the host must \
         announce 64-bit"
    );
    // Torn down before the next load, so its `effClose` cannot land between
    // the second load and the read below.
    drop(instance);

    // The complement, so a hardcoded `1` cannot satisfy the assertion above.
    let (instance, _scratch, path) = load_probe(false);
    let cap = read_capture(&path);
    assert_eq!(cap.set_precision_count, 1);
    assert_eq!(
        cap.set_precision_value, 0,
        "the plugin declared no f64 support, so the host must announce 32-bit"
    );
    drop(instance);
}

/// The opcode is only legal while suspended, and `load` sends it between
/// `effSetBlockSize` and `effMainsChanged(1)`. A resume observed before it
/// means the host announced the width to a running plugin.
#[test]
fn set_process_precision_precedes_the_load_resume() {
    let _guard = lock_probe();
    let (_instance, _scratch, path) = load_probe(true);

    let cap = read_capture(&path);
    assert_eq!(cap.set_precision_count, 1);
    assert_eq!(
        cap.resume_count, 1,
        "load must resume exactly once, after the precision announcement"
    );
    assert_eq!(
        cap.suspend_count, 0,
        "load must not suspend — the precision opcode goes in the window \
         before the first resume, not around one"
    );
}
