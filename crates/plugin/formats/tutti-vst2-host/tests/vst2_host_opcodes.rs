//! Which `audioMaster*` opcodes actually reach this host's `Host` impl?
//!
//! `host_dispatch` matches a subset and lets everything else fall through to a
//! `trace!` that returns 0. Two opcodes were **wired but unreachable**, which is
//! worse than being absent: the `Host` methods existed, one of them overridden
//! with a real value, and no arm ever called them. A plugin got 0 and the host
//! looked like it had support it did not.
//!
//! - `audioMasterUpdateDisplay` (42) — fired after a preset or program change
//!   from the plugin's own editor. `Host::update_display` was declared with
//!   nothing routing to it, so the host's cached parameter view silently drifted
//!   from the plugin's.
//! - `audioMasterCurrentId` (2) — a shell plugin asks this during
//!   `VSTPluginMain` to learn which sub-plugin to become. Returning 0 means "no
//!   particular id", so every shell loaded its default effect — while
//!   `Host::get_plugin_id` was overridden to `'DAWI'` and never consulted.
//!
//! `audioMasterGetLanguage` (38) is the third: the fall-through's 0 is not a
//! valid `HostLanguage`, which is 1-based, so a plugin indexing a string table
//! by it reads slot 0.
//!
//! # What is covered here, and what is not
//!
//! Only `audioMasterUpdateDisplay`. The other two arms are landed but not
//! witnessed, and cannot be by this fixture:
//!
//! - `audioMasterCurrentId` is asked from inside `VSTPluginMain`, before the
//!   host has an instance to observe through — a shell plugin uses the answer
//!   to decide *what to become*. Covering it needs a probe that is a shell,
//!   which is a different fixture, not a switch on this one.
//! - `audioMasterGetLanguage` is never asked by this probe, and adding a call
//!   would only assert that the probe asks — the value reaching a plugin's
//!   string table is not observable from the host side.
//!
//! Both are one-line arms returning a value the `Host` impl already computes,
//! so the risk they carry is small; saying that is better than an assertion
//! that cannot fail.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use tutti_vst2_host::Vst2Instance;

#[path = "support/probe_path.rs"]
mod probe_path;

const SAMPLE_RATE: f64 = 44_100.0;
const BLOCK: usize = 512;

/// Serializes the probe's process-global switches across load→assert.
static PROBE_LOCK: Mutex<()> = Mutex::new(());

/// Reach a `#[no_mangle]` switch in the probe.
///
/// The probe is a cdylib the host `dlopen`s at run time rather than something
/// this binary links, so its exports are resolved by re-opening the same file.
fn set_fire_update_display(enable: bool) {
    let path = Path::new(probe_path::probe_path());
    // SAFETY: the path is the cdylib this crate's dev-dependency built.
    let lib = unsafe { libloading::Library::new(path) }
        .unwrap_or_else(|e| panic!("re-open reference plugin at {path:?}: {e}"));
    // SAFETY: the probe exports this as `extern "C" fn(bool)`.
    let sym: libloading::Symbol<*mut std::ffi::c_void> =
        unsafe { lib.get(b"tutti_vst2_probe_set_fire_update_display\0") }
            .expect("probe missing symbol tutti_vst2_probe_set_fire_update_display");
    let f: extern "C" fn(bool) = unsafe { std::mem::transmute(*sym) };
    f(enable);

    // Leak the handle, deliberately. The switch this just set is a `static` in
    // the probe's image, and it only survives while that image stays mapped —
    // dropping `lib` decrements the refcount that keeps it mapped. With no
    // `Vst2Instance` holding the probe open at that moment, the value is
    // written and then discarded with the unload, and the next load maps a
    // fresh image reading the default.
    //
    // Measured on the same bug in `vst2_latency.rs`: 2 of 6 runs failed without
    // this, 0 of 6 with it. It reads as flakiness because it passes whenever
    // another test's instance happens to keep the image resident — which is why
    // `update_display_reaches_the_host` here had also been written off as flaky.
    std::mem::forget(lib);
}

/// Holds the lock and clears the switch on drop, so a failing assertion cannot
/// leave a display-firing probe behind for the next suite in this binary.
struct Probe {
    _lock: MutexGuard<'static, ()>,
}

impl Probe {
    fn acquire() -> Self {
        let lock = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        set_fire_update_display(false);
        Probe { _lock: lock }
    }

    fn fires_update_display(&self) {
        set_fire_update_display(true);
    }

    fn load(&self) -> Vst2Instance {
        Vst2Instance::load(probe_path::probe_path(), SAMPLE_RATE, BLOCK)
            .expect("the probe must load")
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        set_fire_update_display(false);
    }
}

/// `audioMasterUpdateDisplay` reaches the host and raises the stale flag.
///
/// The probe fires it from `resume`, which `Vst2Instance::load` dispatches, so
/// a loaded instance has already seen one. Before the routing arm existed the
/// opcode hit the `trace!` fall-through and the flag stayed clear.
#[test]
fn update_display_reaches_the_host() {
    let probe = Probe::acquire();
    probe.fires_update_display();

    let instance = probe.load();

    assert!(
        instance.take_display_stale(),
        "audioMasterUpdateDisplay fell through to the unhandled-opcode arm, so \
         the host never learned its parameter view was stale"
    );
}

/// The flag is consumed, so one request produces one re-read.
///
/// A latch that stayed set would make every later poll re-read the whole
/// parameter list for a change already handled.
#[test]
fn the_stale_flag_is_consumed_by_reading_it() {
    let probe = Probe::acquire();
    probe.fires_update_display();

    let instance = probe.load();

    assert!(instance.take_display_stale(), "first read sees the request");
    assert!(
        !instance.take_display_stale(),
        "the flag must clear on read, or a single request re-reads forever"
    );
}

/// A plugin that asks for nothing leaves the flag clear.
///
/// The negative half: without it, a host that always reported "stale" would
/// pass the test above.
#[test]
fn a_quiet_plugin_leaves_the_display_flag_clear() {
    let probe = Probe::acquire();
    // No `fires_update_display` — the probe's default.

    let instance = probe.load();

    assert!(
        !instance.take_display_stale(),
        "nothing asked for a refresh, so nothing should be reported"
    );
}
