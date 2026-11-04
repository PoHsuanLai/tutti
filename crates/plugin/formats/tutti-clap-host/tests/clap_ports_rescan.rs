//! What the host caches from the plugin's `clap.audio-ports` and `clap.tail`,
//! and when it reads them again.
//!
//! The 64-bit flag lives on each audio port, and a plugin may change its
//! ports (CLAP's `audio-ports.rescan`) while deactivated. The host read the
//! flag once, at load, so a plugin that changed it was activated, or refused,
//! on the answer it gave at load. The tail is a separate extension a plugin
//! may not implement at all, which is not the same answer as a tail of 0.
//!
//! The reference plugin's switches are process-globals inside the cdylib, so
//! they are reached through `dlopen` handles, as the other suites here do.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

mod support;
use support::probe_path::probe_path;

use tutti_clap_host::{ClapError, ClapLoaded};
use tutti_clap_test_plugin::{PORTS_64BIT_INPUTS, PORTS_64BIT_OUTPUTS, REPORTED_TAIL_SAMPLES};

static PROBE_LOCK: Mutex<()> = Mutex::new(());

const BOTH_SIDES: u32 = PORTS_64BIT_INPUTS | PORTS_64BIT_OUTPUTS;

/// A held lock and the probe's switches; the defaults come back on drop.
struct Probe {
    _lock: MutexGuard<'static, ()>,
    _lib: libloading::Library,
    set_ports_64bit: unsafe extern "C" fn(u32),
    set_tail_extension: unsafe extern "C" fn(u32),
}

impl Probe {
    fn acquire() -> Self {
        let lock = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unsafe {
            let lib = libloading::Library::new(probe_path()).expect("re-open reference plugin");
            let set_ports_64bit = *lib
                .get::<unsafe extern "C" fn(u32)>(b"tutti_test_plugin_set_ports_64bit\0")
                .expect("set_ports_64bit symbol present");
            let set_tail_extension = *lib
                .get::<unsafe extern "C" fn(u32)>(b"tutti_test_plugin_set_tail_extension\0")
                .expect("set_tail_extension symbol present");
            set_ports_64bit(0);
            set_tail_extension(1);
            Probe {
                _lock: lock,
                _lib: lib,
                set_ports_64bit,
                set_tail_extension,
            }
        }
    }

    fn ports_64bit(&self, mask: u32) {
        unsafe { (self.set_ports_64bit)(mask) }
    }

    fn tail_extension(&self, present: bool) {
        unsafe { (self.set_tail_extension)(u32::from(present)) }
    }

    fn load(&self) -> ClapLoaded {
        let path = Path::new(probe_path());
        ClapLoaded::load_with_library(path, Some(path), 48_000.0, 512)
            .expect("reference plugin should load")
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        unsafe {
            (self.set_ports_64bit)(0);
            (self.set_tail_extension)(1);
        }
    }
}

/// **A rescan refreshes the 64-bit flag, both ways.** The flag was read at
/// load and never again, so it stayed whatever the plugin said then.
///
/// Mutation (run): delete `self.audio.supports_f64 = supports_f64;` in
/// `ClapLoaded::rescan_audio_ports` → the first `assert!` fails.
#[test]
fn a_rescan_rereads_the_f64_flag() {
    let probe = Probe::acquire();
    let mut loaded = probe.load();
    assert!(!loaded.supports_f64(), "fixture: f32-only at load");

    probe.ports_64bit(BOTH_SIDES);
    loaded.rescan_audio_ports().expect("deactivated");
    assert!(loaded.supports_f64(), "the plugin now flags its ports");

    probe.ports_64bit(PORTS_64BIT_OUTPUTS);
    loaded.rescan_audio_ports().expect("deactivated");
    assert!(
        !loaded.supports_f64(),
        "one unflagged side vetoes 64-bit, as at load"
    );
}

/// **A plugin brought back through deactivate and activate is read afresh.**
/// It is the reload a host does when the plugin asked for a restart, and the
/// one nobody has to remember to rescan for.
///
/// Mutation (run): delete the `rescan_audio_ports` call in
/// `ClapLoaded::activate` → the `f64` activation is refused on the stale
/// flag, and the second one is not → both `expect`s fail.
#[test]
fn activation_rereads_the_f64_flag() {
    let probe = Probe::acquire();
    let loaded = probe.load();
    assert!(!loaded.supports_f64());

    probe.ports_64bit(BOTH_SIDES);
    let active = loaded
        .activate::<f64>()
        .map_err(|(_, e)| e)
        .expect("the plugin flags its ports now, so 64-bit activation is allowed");

    let loaded = active.deactivate();
    probe.ports_64bit(0);
    match loaded.activate::<f64>() {
        Err((_, ClapError::NotSupported(_))) => {}
        Err((_, e)) => panic!("expected NotSupported, got {e}"),
        Ok(_) => panic!("the plugin dropped its 64-bit flags, so 64-bit activation is refused"),
    }
}

/// **No `clap.tail` is an absent answer, not a tail of 0.**
///
/// Mutation (run): make `get_tail` return `Some(0)` for a null extension →
/// the first assertion fails.
#[test]
fn a_plugin_without_clap_tail_has_no_answer() {
    let probe = Probe::acquire();

    probe.tail_extension(false);
    assert_eq!(probe.load().get_tail(), None);

    probe.tail_extension(true);
    assert_eq!(probe.load().get_tail(), Some(REPORTED_TAIL_SAMPLES));
}
