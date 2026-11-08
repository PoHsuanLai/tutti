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
use support::active::{Activate, Active};
use support::main_thread;
use support::probe_path::probe_path;

use assert_no_alloc::{AllocDisabler, assert_no_alloc};
use tutti_clap_host::{AudioBuffer32, ClapError, ClapLoaded, ClapProcessContext, LoadOptions};
use tutti_clap_test_plugin::{PORTS_64BIT_INPUTS, PORTS_64BIT_OUTPUTS, REPORTED_TAIL_SAMPLES};
use tutti_plugin_types::PluginError;

// Makes `assert_no_alloc` abort on an allocation; without it the no-alloc
// assertions below would pass unconditionally.
#[global_allocator]
static A: AllocDisabler = AllocDisabler;

static PROBE_LOCK: Mutex<()> = Mutex::new(());

const BOTH_SIDES: u32 = PORTS_64BIT_INPUTS | PORTS_64BIT_OUTPUTS;

/// A held lock and the probe's switches; the defaults come back on drop.
struct Probe {
    _lock: MutexGuard<'static, ()>,
    _lib: libloading::Library,
    set_ports_64bit: unsafe extern "C" fn(u32),
    set_tail_extension: unsafe extern "C" fn(u32),
    set_port_layout: unsafe extern "C" fn(u32),
    inactive_deactivations: unsafe extern "C" fn() -> u32,
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
            let reset_activation = *lib
                .get::<unsafe extern "C" fn()>(b"tutti_test_plugin_reset_activation\0")
                .expect("reset_activation symbol present");
            let inactive_deactivations = *lib
                .get::<unsafe extern "C" fn() -> u32>(b"tutti_test_plugin_inactive_deactivations\0")
                .expect("inactive_deactivations symbol present");
            set_ports_64bit(0);
            set_tail_extension(1);
            set_port_layout(0);
            reset_activation();
            Probe {
                _lock: lock,
                _lib: lib,
                set_ports_64bit,
                set_tail_extension,
                set_port_layout,
                inactive_deactivations,
            }
        }
    }

    /// How often the host called `deactivate` on a plugin that was inactive.
    fn inactive_deactivations(&self) -> u32 {
        unsafe { (self.inactive_deactivations)() }
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
        ClapLoaded::load(path, LoadOptions::new().library(path))
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
    loaded
        .rescan_audio_ports(&main_thread())
        .expect("deactivated");
    assert!(loaded.supports_f64(), "the plugin now flags its ports");

    probe.ports_64bit(PORTS_64BIT_OUTPUTS);
    loaded
        .rescan_audio_ports(&main_thread())
        .expect("deactivated");
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
/// `ClapLoaded::activate_now` → the `f64` activation is refused on the stale
/// flag, and the second one is not → both `expect`s fail.
#[test]
fn activation_rereads_the_f64_flag() {
    let probe = Probe::acquire();
    let loaded = probe.load();
    assert!(!loaded.supports_f64());

    probe.ports_64bit(BOTH_SIDES);
    let active = loaded
        .activate_at::<f64>(48_000.0, 512)
        .map_err(|(_, e)| e)
        .expect("the plugin flags its ports now, so 64-bit activation is allowed");

    let loaded = active.deactivate();
    probe.ports_64bit(0);
    match loaded.activate_at::<f64>(48_000.0, 512) {
        Err((_, PluginError::Other(reason))) => {
            assert!(reason.contains("64-bit"), "{reason}");
        }
        Err((_, e)) => panic!("expected a refusal naming 64-bit audio, got {e}"),
        Ok(_) => panic!("the plugin dropped its 64-bit flags, so 64-bit activation is refused"),
    }
}

/// **No `clap.tail` is an absent answer, not a tail of 0.**
///
/// Mutation (run): in `ClapLoaded::tail_samples`, replace the `return None;` under
/// `if self.extensions.system.tail.is_null()` with `return Some(0);` → the
/// first assertion fails.
#[test]
fn a_plugin_without_clap_tail_has_no_answer() {
    let probe = Probe::acquire();

    probe.tail_extension(false);
    assert_eq!(probe.load().tail_samples(), None);

    probe.tail_extension(true);
    assert_eq!(probe.load().tail_samples(), Some(REPORTED_TAIL_SAMPLES));
}

/// **A rescan of an active plugin is refused.** CLAP lets a plugin change its
/// ports only while deactivated, so a layout read under an active plugin would
/// pair with buffers sized for the old one. The Controller derefs mutably to
/// `ClapLoaded`, so the call is reachable.
///
/// Mutation (run): change the guard in `ClapLoaded::rescan_audio_ports` to
/// `if false && self.flags.active` → the call succeeds → `expect_err` fails.
#[test]
fn a_rescan_of_an_active_plugin_is_refused() {
    let probe = Probe::acquire();
    let mut active = probe
        .load()
        .activate_at::<f32>(48_000.0, 512)
        .map_err(|(_, e)| e)
        .expect("activates");

    probe.ports_64bit(BOTH_SIDES);
    let err = active
        .rescan_audio_ports(&main_thread())
        .expect_err("the plugin is active");
    assert!(matches!(err, ClapError::NotSupported(_)), "got {err}");
    assert!(
        !active.supports_f64(),
        "a refused rescan reads nothing: the flag stays what activation read"
    );
}

/// **A re-activation reads the ports again and sizes its buffers from them.**
/// A rate change deactivates and activates the plugin, which may have changed
/// its layout in between; the old layout would pair a `[2, 1]` plugin with
/// scratch for one stereo bus.
///
/// Mutation (run): delete the `self.rescan_audio_ports()` call in
/// `ClapLoaded::activate_now` → the port count stays 1 → the first assertion
/// fails.
#[test]
fn a_rate_change_rereads_the_port_layout() {
    let probe = Probe::acquire();
    let mut active = probe
        .load()
        .activate_at::<f32>(48_000.0, 512)
        .map_err(|(_, e)| e)
        .expect("activates");
    assert_eq!(active.audio_port_count(true), 1, "fixture: one input port");
    assert_eq!(active.info().audio_inputs, 2);

    probe.port_layout(LAYOUT_ASYMMETRIC_AUX);
    active.reconfigure(44_100.0, 512).expect("re-activates");

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

/// Runs one block of `channels` channels through a 32-bit instance inside
/// `assert_no_alloc`, returning the instance's answer.
fn process_f32_block(active: &mut Active<f32>, channels: usize) -> Result<(), ClapError> {
    const FRAMES: usize = 64;
    let ins = vec![vec![0.5f32; FRAMES]; channels];
    let in_refs: Vec<&[f32]> = ins.iter().map(Vec::as_slice).collect();
    let mut outs = vec![vec![0.0f32; FRAMES]; channels];
    let mut out_refs: Vec<&mut [f32]> = outs.iter_mut().map(Vec::as_mut_slice).collect();
    let mut buffer = AudioBuffer32 {
        inputs: &in_refs,
        outputs: &mut out_refs,
        num_samples: FRAMES,
        sample_rate: 44_100.0,
    };
    let ctx = ClapProcessContext::default();
    assert_no_alloc(|| active.process(&mut buffer, &ctx).map(|_| ()))
}

/// **A 64-bit activation whose plugin dropped its flags is refused, and the
/// refusal hands back the plugin deactivated and untouched.** The plugin is not
/// active (CLAP allows `process` only while active), nothing is deactivated a
/// second time, and the same instance then activates at 32-bit.
///
/// Mutations (run): delete the `wide && !self.audio.supports_f64` refusal in
/// `ClapLoaded::activate_now` → the plugin is activated for 64-bit audio on
/// ports that do not take them → the `Err` match fails. Return `None` for the
/// plugin in the `refuse` closure → the `expect` on the returned plugin fails.
/// Remove the `flags.active` guard in `ClapLoaded::deactivate_plugin` → the
/// retire of the later activation deactivates an inactive plugin → the final
/// count assertion fails.
#[test]
fn an_f64_activation_on_dropped_flags_is_refused_with_the_plugin_intact() {
    let probe = Probe::acquire();
    probe.ports_64bit(BOTH_SIDES);
    let active = probe
        .load()
        .activate_at::<f64>(48_000.0, 512)
        .map_err(|(_, e)| e)
        .expect("flagged, so 64-bit activation is allowed");
    let loaded = active.deactivate();

    probe.ports_64bit(0);
    let (loaded, reason) = loaded
        .activate_at::<f64>(44_100.0, 512)
        .expect_err("the flags are gone");
    assert!(matches!(reason, PluginError::Other(_)), "got {reason}");
    assert!(
        !reason.to_string().contains("at :"),
        "no empty path in the message: {reason}"
    );
    assert!(!loaded.is_active(), "left deactivated");

    let mut active = loaded
        .activate_at::<f32>(44_100.0, 512)
        .map_err(|(_, e)| e)
        .expect("the same instance runs at 32-bit");
    assert!(active.is_active());
    process_f32_block(&mut active, 2).expect("and renders");
    drop(active);
    assert_eq!(
        probe.inactive_deactivations(),
        0,
        "no `deactivate` reached a plugin that was inactive"
    );
}

/// **A refused activation that also changed the port layout leaves the ports
/// committed, and the next activation sizes its buffers from them.** The plugin
/// drops its 64-bit flags and switches to the `[2, 1]` layout in the same
/// rescan; a 3-channel block must index the new scratch, not a stereo one (a
/// panic) or grow the pointer vectors (an allocation on the audio path).
///
/// Mutation (run): size the scratch in `activate_now` from a layout read
/// before the rescan → the 3-channel block indexes past the scratch or
/// allocates → this test aborts (SIGABRT, "memory allocation failed" under
/// `assert_no_alloc`).
#[test]
fn a_refused_activation_with_a_new_layout_leaves_ports_the_next_one_fits() {
    let probe = Probe::acquire();
    probe.ports_64bit(BOTH_SIDES);
    let active = probe
        .load()
        .activate_at::<f64>(48_000.0, 512)
        .map_err(|(_, e)| e)
        .expect("flagged, so 64-bit activation is allowed");
    assert_eq!(active.audio_port_count(true), 1, "fixture: one input port");
    let loaded = active.deactivate();

    probe.ports_64bit(0);
    probe.port_layout(LAYOUT_ASYMMETRIC_AUX);
    let (loaded, _) = loaded
        .activate_at::<f64>(44_100.0, 512)
        .expect_err("the flags are gone");
    assert_eq!(
        loaded.audio_port_count(true),
        2,
        "the rescan is committed, so the layout is the plugin's"
    );

    let mut active = loaded
        .activate_at::<f32>(44_100.0, 512)
        .map_err(|(_, e)| e)
        .expect("activates at 32-bit");
    process_f32_block(&mut active, 3).expect("a block in the new layout processes");
}

/// **A layout change across a successful rate change shows in the channel
/// totals the host sized its scratch from.** The instance keeps running and
/// processes the new width.
///
/// Mutation (run): delete the `self.loaded.rescan_audio_ports()?;` line in
/// `ClapLoaded::activate_now` → the totals are unchanged → the assertion fails.
#[test]
fn a_successful_rate_change_exposes_the_new_layout() {
    let probe = Probe::acquire();
    let mut active = probe
        .load()
        .activate_at::<f32>(48_000.0, 512)
        .map_err(|(_, e)| e)
        .expect("activates");
    let before = (active.info().audio_inputs, active.info().audio_outputs);

    probe.port_layout(LAYOUT_ASYMMETRIC_AUX);
    active.reconfigure(44_100.0, 512).expect("re-activates");

    let after = (active.info().audio_inputs, active.info().audio_outputs);
    assert_ne!(before, after, "the totals follow the plugin's new layout");
    assert!(active.is_active());
}

/// **A deactivate and activate at the rate and block size the plugin runs at
/// re-reads the ports**, which is what serving a plugin's `audio-ports.rescan`
/// or `request_restart` needs. Nothing is re-read before it.
///
/// Mutation (run): make the helper's `reactivate` return `Ok(())` without
/// deactivating → the cached input total stays 2 → fails.
#[test]
fn reactivate_rereads_the_port_layout_in_place() {
    let probe = Probe::acquire();
    let mut active = probe
        .load()
        .activate_at::<f32>(48_000.0, 512)
        .map_err(|(_, e)| e)
        .expect("activates");
    assert_eq!(active.info().audio_inputs, 2, "fixture: one stereo input");

    probe.port_layout(LAYOUT_ASYMMETRIC_AUX);
    assert_eq!(active.info().audio_inputs, 2, "nothing was re-read");

    active.reactivate().expect("re-activates");
    assert!(active.is_active());
    assert_eq!(active.info().audio_inputs, 3, "the plugin's new layout");
    assert_eq!(active.sample_rate(), 48_000.0);
    assert_eq!(active.block_size(), 512);
}
