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

use assert_no_alloc::{assert_no_alloc, AllocDisabler};
use tutti_clap_host::{AudioBuffer32, AudioBuffer64, ClapError, ClapLoaded, ClapProcessContext};
use tutti_clap_test_plugin::{PORTS_64BIT_INPUTS, PORTS_64BIT_OUTPUTS, REPORTED_TAIL_SAMPLES};

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

/// Runs one block of `channels` channels through a 64-bit instance inside
/// `assert_no_alloc`, returning the instance's answer.
fn process_f64_block(
    active: &mut tutti_clap_host::ClapActive<f64>,
    channels: usize,
) -> Result<(), ClapError> {
    const FRAMES: usize = 64;
    let ins = vec![vec![0.5f64; FRAMES]; channels];
    let in_refs: Vec<&[f64]> = ins.iter().map(Vec::as_slice).collect();
    let mut outs = vec![vec![0.0f64; FRAMES]; channels];
    let mut out_refs: Vec<&mut [f64]> = outs.iter_mut().map(Vec::as_mut_slice).collect();
    let mut buffer = AudioBuffer64 {
        inputs: &in_refs,
        outputs: &mut out_refs,
        num_samples: FRAMES,
        sample_rate: 44_100.0,
    };
    let ctx = ClapProcessContext::default();
    assert_no_alloc(|| active.process(&mut buffer, &ctx).map(|_| ()))
}

/// **A 64-bit instance whose plugin drops its flags is not re-activated for
/// 64-bit audio, and what it is left as refuses to process.** The refusal is
/// [`ClapError::Deactivated`], the instance stays deactivated, as after a
/// refused rollback, and `process` returns [`ClapError::NotActive`] without
/// calling the plugin (CLAP allows `process` only while active), allocating, or
/// starting processing.
///
/// Mutations (run): delete the `T::requires_f64() && ...` check in
/// `ClapActive::reconfigure` → the plugin is re-activated for 64-bit audio on
/// ports that do not take them → `expect_err` fails. Replace the
/// `!self.loaded.flags.active` conditions in both `ClapActive::process_impl`
/// and `ensure_processing` with `false` → the block reaches the deactivated
/// plugin → the `NotActive` assertion fails. Either check alone keeps the
/// test green: the second is the one that guards the FFI call. Drop the
/// `self.loaded.flags.active &&` from `set_sample_rate`'s same-rate early
/// return → the retry at the old rate returns `Ok` on a deactivated instance →
/// the retry's `expect_err` fails.
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
    assert!(matches!(err, ClapError::Deactivated(_)), "got {err}");
    assert!(
        !err.to_string().contains("at :"),
        "no empty path in the message: {err}"
    );

    assert!(!active.is_active(), "left deactivated");
    let refused = process_f64_block(&mut active, 2).expect_err("a deactivated plugin is not run");
    assert!(matches!(refused, ClapError::NotActive), "got {refused}");
    assert!(!active.is_processing(), "processing was never started");

    let retry = active
        .set_sample_rate(48_000.0)
        .expect_err("a retry at the old rate still meets dropped flags");
    assert!(matches!(retry, ClapError::Deactivated(_)), "got {retry}");
    probe.ports_64bit(BOTH_SIDES);
    active
        .set_sample_rate(48_000.0)
        .expect("the flags are back, so the same rate re-activates");
    assert!(active.is_active(), "re-activated at the old rate");
}

/// **A failed reconfiguration that also changed the port layout leaves the
/// ports and the scratch agreeing, and a block of any width is refused.** The
/// plugin drops its 64-bit flags and switches to the `[2, 1]` layout in the
/// same rescan; the rescan was committed but the scratch kept its stereo size,
/// so a 3-channel block indexed past it (a panic) or grew the pointer vectors
/// (an allocation on the audio path).
///
/// Mutations (run): replace the `!self.loaded.flags.active` conditions in
/// `process_impl` and `ensure_processing` with `false`, and delete the
/// `self.resize_scratch(prev_frames)` before the `Deactivated` return in
/// `ClapActive::reconfigure` → the block indexes the stale scratch
/// (`refill_port_buffers`) or allocates → this test aborts (SIGABRT, "memory
/// allocation failed" under `assert_no_alloc`). Removing only the two checks
/// stays green, because the resized scratch fits the new layout, and removing
/// only the resize stays green, because the checks refuse first: each is
/// defence for the other.
#[test]
fn a_failed_reconfiguration_with_a_new_layout_refuses_blocks_of_any_width() {
    let probe = Probe::acquire();
    probe.ports_64bit(BOTH_SIDES);
    let mut active = probe
        .load()
        .activate::<f64>()
        .map_err(|(_, e)| e)
        .expect("flagged, so 64-bit activation is allowed");
    assert_eq!(active.audio_port_count(true), 1, "fixture: one input port");

    probe.ports_64bit(0);
    probe.port_layout(LAYOUT_ASYMMETRIC_AUX);
    active
        .set_sample_rate(44_100.0)
        .expect_err("the flags are gone");

    assert_eq!(
        active.audio_port_count(true),
        2,
        "the rescan is committed, so the layout is the plugin's"
    );
    for channels in [2, 3] {
        let refused =
            process_f64_block(&mut active, channels).expect_err("a deactivated plugin is not run");
        assert!(
            matches!(refused, ClapError::NotActive),
            "{channels} channels: got {refused}"
        );
    }
}

/// **A layout change across a successful rate change shows in the channel
/// totals the host sized its scratch from.** The instance keeps running and
/// processes the new width.
///
/// Mutation (run): delete the `self.loaded.rescan_audio_ports()?;` line in
/// `ClapActive::reconfigure` → the totals are unchanged → the assertion fails.
#[test]
fn a_successful_rate_change_exposes_the_new_layout() {
    let probe = Probe::acquire();
    let mut active = probe
        .load()
        .activate::<f32>()
        .map_err(|(_, e)| e)
        .expect("activates");
    let before = (active.info().audio_inputs, active.info().audio_outputs);

    probe.port_layout(LAYOUT_ASYMMETRIC_AUX);
    active.set_sample_rate(44_100.0).expect("re-activates");

    let after = (active.info().audio_inputs, active.info().audio_outputs);
    assert_ne!(before, after, "the totals follow the plugin's new layout");
    assert!(active.is_active());
}
