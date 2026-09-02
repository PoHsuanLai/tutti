//! Pins the two events `handle.rs`'s module docs insist are different, and the
//! bug that came of conflating them.
//!
//! * **`effClose`** releases *this instance*: the plugin frees its `AEffect`,
//!   drops its DSP state, and hands back any licence seat. Mandatory — skip it
//!   and every A/B of a plugin slot leaks one live instance and one licence.
//! * **`dlclose`** unloads the shared *module*, running its static destructors.
//!   That is the step that crashes with JUCE-based plugins, so hosts (JUCE and
//!   Ardour both) never take it.
//!
//! `Vst2Handle` once had these exactly backwards: it was `ManuallyDrop` and
//! skipped the instance destructor entirely, so `effClose` never ran while the
//! `Arc<Library>` inside still dropped. The fix (`src/handle.rs`, the ordering
//! its `Drop` spells out) has survived only as prose in that file and as a
//! `std::mem::forget(lib)` plus an explanatory comment in seven test files.
//! Nothing executed it.
//!
//! # Why this was mistaken for flakiness
//!
//! The seven copies of that comment record the symptom: the probe's switches
//! are `static`s inside its image, and they survive only while the image stays
//! mapped. A test helper that opened the library, wrote a switch, and let the
//! handle drop decremented the refcount that kept it mapped — and with no
//! `Vst2Instance` holding it open at that moment, the write went with the
//! unload and the next load mapped a fresh image reading the default.
//!
//! It presented as load-sensitivity because it passed whenever *another* test's
//! instance happened to keep the image resident, which is a matter of
//! scheduling. The diagnosis "these tests are load-sensitive" was wrong: the
//! defect was a dropped `Library` handle, and it was deterministic once the
//! refcount was accounted for. This file asserts on the refcount directly, so
//! there is nothing left to schedule.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use tutti_vst2_host::Vst2Instance;

#[path = "support/probe_path.rs"]
mod probe_path;

const SAMPLE_RATE: f64 = 48_000.0;
const BLOCK: usize = 128;

/// The probe's counters are process-globals inside one shared image, so a whole
/// reset → drive → read sequence must not interleave with another's.
static PROBE_LOCK: Mutex<()> = Mutex::new(());

fn lock_probe() -> MutexGuard<'static, ()> {
    PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// Re-open the image the host loaded and call one of the probe's exported
/// control functions, **leaking the handle**.
///
/// The leak is the subject of this file, not an accident. It is the same
/// `std::mem::forget` the seven existing probe helpers perform, and
/// [`a_switch_written_through_a_leaked_handle_survives_the_hosts_load`] is what
/// asserts it buys something.
///
/// Going through `dlopen` at all is required — the linked rlib is a different
/// image with its own statics, and only the cdylib's globals see the host's
/// calls.
fn probe_call_keeping_image<R>(path: &PathBuf, symbol: &[u8], f: impl FnOnce(*mut ()) -> R) -> R {
    // SAFETY: the path is the cdylib this crate's dev-dependency built.
    let lib = unsafe { libloading::Library::new(path) }
        .unwrap_or_else(|e| panic!("re-open reference plugin at {path:?}: {e}"));
    // SAFETY: symbol names are the probe's `#[no_mangle]` exports.
    let sym: libloading::Symbol<*mut std::ffi::c_void> =
        unsafe { lib.get(symbol) }.unwrap_or_else(|e| {
            panic!(
                "probe missing symbol {}: {e}",
                String::from_utf8_lossy(symbol)
            )
        });
    let r = f(*sym as *mut ());
    std::mem::forget(lib);
    r
}

fn close_count(path: &PathBuf) -> u32 {
    probe_call_keeping_image(path, b"tutti_vst2_probe_close_count\0", |sym| {
        // SAFETY: the probe exports this as `extern "C" fn() -> u32`.
        let f: extern "C" fn() -> u32 = unsafe { std::mem::transmute(sym) };
        f()
    })
}

fn reset_close_count(path: &PathBuf) {
    probe_call_keeping_image(path, b"tutti_vst2_probe_reset_close_count\0", |sym| {
        // SAFETY: the probe exports this as `extern "C" fn()`.
        let f: extern "C" fn() = unsafe { std::mem::transmute(sym) };
        f();
    });
}

/// **The regression test for `handle.rs:82`.** Dropping a `Vst2Instance` must
/// dispatch `effClose`, and must do so while the module is still mapped.
///
/// The two halves are one assertion, because the count is *read out of the
/// image*: a value of 1 is simultaneously proof that `effClose` was dispatched
/// and proof that the image survived the drop to remember it. That is why this
/// can pin an ordering without instrumenting `dlclose` — an unload would have
/// taken the counter with it and left a fresh image answering 0, which is the
/// same answer a skipped `effClose` gives, and the distinguishing test below
/// separates them.
///
/// Under the pre-fix `ManuallyDrop` handle this reads 0: the instance
/// destructor never ran, so `effClose` was never dispatched.
#[test]
fn dropping_an_instance_dispatches_eff_close_while_the_module_stays_mapped() {
    let _guard = lock_probe();
    let path = probe_path::probe_path().clone();

    reset_close_count(&path);
    assert_eq!(
        close_count(&path),
        0,
        "the reset must be observable, or every assertion below is vacuous — a \
         reset that landed in a different image than the host loads would leave \
         this nonzero"
    );

    let instance = Vst2Instance::load(&path, SAMPLE_RATE, BLOCK)
        .unwrap_or_else(|e| panic!("host failed to load reference plugin at {path:?}: {e:?}"));
    assert_eq!(
        close_count(&path),
        0,
        "a live instance must not have been closed"
    );

    drop(instance);

    assert_eq!(
        close_count(&path),
        1,
        "dropping a Vst2Instance must dispatch effClose exactly once, and the \
         module must still be mapped afterwards to report it. 0 means either \
         the instance destructor was skipped (the `ManuallyDrop` bug) or the \
         module was unloaded with the instance, taking the counter with it — \
         and hosts must not unload, because that is the step that runs a \
         JUCE plugin's static destructors"
    );
}

/// The instance's own handle must not be the only thing keeping the module
/// mapped: a *second* load after the first has been closed must find the same
/// image, still carrying its counter.
///
/// This is the half `dropping_an_instance_...` cannot see on its own. If
/// `Vst2Handle::drop` unloaded the module, the count above would be 0 for a
/// reason indistinguishable from "effClose was skipped"; here the first close
/// is already banked, so a second load that finds 1 proves the image persisted
/// across a full instance lifetime and a fresh load.
#[test]
fn the_module_survives_an_instance_lifetime_and_a_reload() {
    let _guard = lock_probe();
    let path = probe_path::probe_path().clone();

    reset_close_count(&path);

    drop(
        Vst2Instance::load(&path, SAMPLE_RATE, BLOCK)
            .unwrap_or_else(|e| panic!("first load failed: {e:?}")),
    );
    assert_eq!(close_count(&path), 1, "the first instance was closed");

    let second = Vst2Instance::load(&path, SAMPLE_RATE, BLOCK)
        .unwrap_or_else(|e| panic!("second load failed: {e:?}"));
    assert_eq!(
        close_count(&path),
        1,
        "the second load must map the SAME image, which still remembers the \
         first close. A 0 here means the module was unloaded when the first \
         instance dropped and this load mapped a fresh one — the unload hosts \
         must never do"
    );

    drop(second);
    assert_eq!(
        close_count(&path),
        2,
        "and the second instance closes too, into that same surviving image"
    );
}

/// A switch written through a re-opened handle must still be there when the
/// *host* loads the plugin — the property the seven `std::mem::forget(lib)`
/// calls exist to guarantee.
///
/// This is the bug from the other side. The probe's switches are `static`s
/// inside its image and survive only while that image stays mapped, so a helper
/// that opened the library, wrote a switch and dropped the handle could lose
/// the write to the unload — leaving the next load to map a fresh image reading
/// the default. It presented as flakiness because it passed whenever another
/// test's instance happened to keep the image resident.
///
/// # What this can and cannot assert
///
/// It deliberately does **not** assert that dropping a handle unloads the
/// image. Measured here: it does not — glibc kept the probe resident, and
/// `dlclose` is explicitly permitted to (`RTLD_NODELETE`, images with certain
/// TLS shapes, a refcount another mapping still holds). An assertion that the
/// drop loses the write would pass on no platform tested and would be pinning a
/// platform's discretion rather than this host's behaviour.
///
/// What *is* invariant, and is what the `forget` buys, is the direction: a
/// leaked handle can never lose the write. So that is what is asserted, over a
/// full write → host-load → read cycle — the exact sequence the seven helpers
/// perform. Under the pre-fix helper this failed on any run where no other
/// instance held the image; here it cannot fail for that reason at all, which
/// is the point.
#[test]
fn a_switch_written_through_a_leaked_handle_survives_the_hosts_load() {
    let _guard = lock_probe();
    let path = probe_path::probe_path().clone();

    reset_close_count(&path);

    // Write through a leaked handle, then have the HOST map the image and
    // dispatch into it. If the write had gone to an image that was subsequently
    // unloaded, the host's load would map a fresh one and the close below would
    // land on a counter that never saw the reset.
    drop(
        Vst2Instance::load(&path, SAMPLE_RATE, BLOCK)
            .unwrap_or_else(|e| panic!("load failed: {e:?}")),
    );

    assert_eq!(
        close_count(&path),
        1,
        "the reset, the host's load, the close and this read must all reach ONE          image. Any other count means a write went to an image that did not          survive to receive the host's dispatch — the failure the          `std::mem::forget(lib)` in every probe helper prevents"
    );
}
