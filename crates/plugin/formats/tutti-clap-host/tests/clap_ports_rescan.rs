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

use tutti_clap_host::{AudioBuffer32, ClapError, ClapLoaded, ClapProcessContext, LoadStage};
use tutti_clap_test_plugin::{PORTS_64BIT_INPUTS, PORTS_64BIT_OUTPUTS, REPORTED_TAIL_SAMPLES};

static PROBE_LOCK: Mutex<()> = Mutex::new(());

const BOTH_SIDES: u32 = PORTS_64BIT_INPUTS | PORTS_64BIT_OUTPUTS;

/// A held lock and the probe's switches; the defaults come back on drop.
struct Probe {
    _lock: MutexGuard<'static, ()>,
    _lib: libloading::Library,
    set_ports_64bit: unsafe extern "C" fn(u32),
    set_tail_extension: unsafe extern "C" fn(u32),
    set_port_layout: unsafe extern "C" fn(u32),
}

/// The reference plugin's `[2, 1]` input and output layout.
const LAYOUT_ASYMMETRIC_AUX: u32 = 1;

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
            let set_port_layout = *lib
                .get::<unsafe extern "C" fn(u32)>(b"tutti_test_plugin_set_port_layout\0")
                .expect("set_port_layout symbol present");
            set_ports_64bit(0);
            set_tail_extension(1);
            set_port_layout(0);
            Probe {
                _lock: lock,
                _lib: lib,
                set_ports_64bit,
                set_tail_extension,
                set_port_layout,
            }
        }
    }

    fn ports_64bit(&self, mask: u32) {
        unsafe { (self.set_ports_64bit)(mask) }
    }

    fn port_layout(&self, mode: u32) {
        unsafe { (self.set_port_layout)(mode) }
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
            (self.set_port_layout)(0);
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
/// Mutation (run): in `ClapLoaded::get_tail`, replace the `return None;` under
/// `if self.extensions.system.tail.is_null()` with `return Some(0);` → the
/// first assertion fails.
#[test]
fn a_plugin_without_clap_tail_has_no_answer() {
    let probe = Probe::acquire();

    probe.tail_extension(false);
    assert_eq!(probe.load().get_tail(), None);

    probe.tail_extension(true);
    assert_eq!(probe.load().get_tail(), Some(REPORTED_TAIL_SAMPLES));
}

/// **A rescan of an active plugin is refused.** CLAP lets a plugin change its
/// ports only while deactivated, so a layout read under an active plugin would
/// pair with buffers sized for the old one. `ClapActive` derefs mutably to
/// `ClapLoaded`, so the call is reachable.
///
/// Mutation (run): change the guard in `ClapLoaded::rescan_audio_ports` to
/// `if false && self.flags.active` → the call succeeds → `expect_err` fails.
#[test]
fn a_rescan_of_an_active_plugin_is_refused() {
    let probe = Probe::acquire();
    let mut active = probe
        .load()
        .activate::<f32>()
        .map_err(|(_, e)| e)
        .expect("activates");

    probe.ports_64bit(BOTH_SIDES);
    let err = active
        .rescan_audio_ports()
        .expect_err("the plugin is active");
    assert!(matches!(err, ClapError::NotSupported(_)), "got {err}");
    assert!(
        !active.supports_f64(),
        "a refused rescan reads nothing: the flag stays what activation read"
    );
}

/// **An in-place re-activation reads the ports again and sizes its buffers
/// from them.** `set_sample_rate` deactivates and activates the plugin, which
/// may have changed its layout in between; the old layout would pair a
/// `[2, 1]` plugin with scratch for one stereo bus.
///
/// Mutation (run): delete the `self.loaded.rescan_audio_ports()?;` line in
/// `ClapActive::reconfigure` → the port count stays 1 → the first assertion
/// fails.
#[test]
fn a_rate_change_rereads_the_port_layout() {
    let probe = Probe::acquire();
    let mut active = probe
        .load()
        .activate::<f32>()
        .map_err(|(_, e)| e)
        .expect("activates");
    assert_eq!(active.audio_port_count(true), 1, "fixture: one input port");
    assert_eq!(active.info().audio_inputs, 2);

    probe.port_layout(LAYOUT_ASYMMETRIC_AUX);
    active.set_sample_rate(44_100.0).expect("re-activates");

    assert_eq!(active.audio_port_count(true), 2, "the plugin's new layout");
    assert_eq!(active.info().audio_inputs, 3, "stereo main + mono aux");

    // The scratch fits the new layout: a block of 3 channels in and out runs.
    const FRAMES: usize = 64;
    let ins = vec![vec![0.5f32; FRAMES]; 3];
    let in_refs: Vec<&[f32]> = ins.iter().map(Vec::as_slice).collect();
    let mut outs = vec![vec![0.0f32; FRAMES]; 3];
    let mut out_refs: Vec<&mut [f32]> = outs.iter_mut().map(Vec::as_mut_slice).collect();
    let mut buffer = AudioBuffer32 {
        inputs: &in_refs,
        outputs: &mut out_refs,
        num_samples: FRAMES,
        sample_rate: 44_100.0,
    };
    active
        .process(&mut buffer, &ClapProcessContext::default())
        .expect("a block in the new layout processes");
}

/// **A 64-bit instance whose plugin drops its flags is not re-activated for
/// 64-bit audio.** The refusal is the activation error, and the instance stays
/// deactivated, as after a refused rollback.
///
/// Mutation (run): delete the `T::requires_f64() && ...` check in
/// `ClapActive::reconfigure` → the plugin is re-activated for 64-bit audio on
/// ports that do not take it → `expect_err` fails.
#[test]
fn a_rate_change_does_not_reactivate_a_64_bit_instance_on_dropped_flags() {
    let probe = Probe::acquire();
    probe.ports_64bit(BOTH_SIDES);
    let mut active = probe
        .load()
        .activate::<f64>()
        .map_err(|(_, e)| e)
        .expect("flagged, so 64-bit activation is allowed");

    probe.ports_64bit(0);
    let err = active
        .set_sample_rate(44_100.0)
        .expect_err("the flags are gone");
    assert!(
        matches!(
            err,
            ClapError::LoadFailed {
                stage: LoadStage::Activation,
                ..
            }
        ),
        "got {err}"
    );
}
