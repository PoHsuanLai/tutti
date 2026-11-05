//! Host-conformance harness for **plugin refusals** — the places where a CLAP
//! plugin returns `false` to mean *"no"* and the host must not read it as
//! *"not applicable"*.
//!
//! Companion to the other conformance suites here, which all drive a plugin
//! that says **yes** to everything, so none of them can see these paths.
//! Every assertion below is about what the host does *after* hearing
//! "no", so the paths under test are unreachable without the reference
//! plugin's `refusal` switches; see that module for what each one models.
//!
//! Nothing waits on a clock. The probe's switches and counters are
//! process-globals, so every test holds [`PROBE_LOCK`] for its whole scenario
//! and resets the probe at the top.

mod support;
use support::active::{Activate, Active};

use std::sync::{Mutex, MutexGuard};

use support::probe_path::probe_path;

use tutti_clap_host::types::StateContext;
use tutti_clap_host::{ClapError, ClapLoaded, LoadOptions};
use tutti_clap_test_plugin::ParamStateCapture;
use tutti_clap_test_plugin::refusal::ACTIVATE_REFUSE_NONE;
use tutti_plugin_types::Control;
use tutti_plugin_types::{LoadStage, PluginError};

/// The sample rate every fixture loads and activates at. The probe accepts it,
/// so it is the configuration a rollback must land back on.
const BASE_RATE: f64 = 48_000.0;
/// The block size every fixture loads at.
const BASE_FRAMES: u32 = 512;

/// The probe's switches and counters are process-globals shared by every test
/// in this binary (one dlopen'd image). Serialize whole scenarios — reset →
/// drive → read — so one test cannot observe another's writes.
static PROBE_LOCK: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// A held [`PROBE_LOCK`] plus a freshly-reset probe. Acquiring one is the only
/// way to touch the probe globals, so the reset cannot race a concurrent test.
struct Probe {
    _lock: MutexGuard<'static, ()>,
}

impl Probe {
    fn acquire() -> Self {
        let lock = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset_activation();
        reset_state_context_refusal();
        param_reset();
        Probe { _lock: lock }
    }

    /// Load the reference plugin through the real host, without activating.
    fn load(&self) -> ClapLoaded {
        let path = std::path::Path::new(probe_path());
        // Bare dylib: pass it as both bundle and library so the host dlopens it
        // directly, no `.clap` bundle structure needed.
        ClapLoaded::load(path, LoadOptions::new().library(path))
            .expect("reference plugin should load")
    }

    /// Load + activate at the base configuration, which the probe accepts.
    fn activate(&self) -> Active<f32> {
        self.load()
            .activate_at::<f32>(BASE_RATE, BASE_FRAMES)
            .map_err(|(_, e)| e)
            .expect("reference plugin should activate")
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        // Leave the switches off for whatever runs next, even on an unwind from
        // a failed assertion — a latched refusal would make an unrelated test
        // fail for a reason it has nothing to do with.
        reset_activation();
        reset_state_context_refusal();
    }
}

// --- exported C symbols, reached across the dlopen seam ---------------------
//
// Opening the same path a second time shares the already-loaded image, so these
// see (and drive) exactly the globals the host's calls touched.

fn set_activate_refusal(refuse_rate_bits: u64, refuse_frames: u32) {
    type F = unsafe extern "C" fn(u64, u32);
    unsafe {
        let lib = libloading::Library::new(probe_path()).expect("re-open reference plugin");
        let f: libloading::Symbol<F> = lib
            .get(b"tutti_test_plugin_set_activate_refusal\0")
            .expect("activate refusal symbol present");
        f(refuse_rate_bits, refuse_frames);
    }
}

fn reset_activation() {
    type F = unsafe extern "C" fn();
    unsafe {
        let lib = libloading::Library::new(probe_path()).expect("re-open reference plugin");
        let f: libloading::Symbol<F> = lib
            .get(b"tutti_test_plugin_reset_activation\0")
            .expect("reset activation symbol present");
        f();
    }
}

fn activate_refusals() -> u32 {
    type F = unsafe extern "C" fn() -> u32;
    unsafe {
        let lib = libloading::Library::new(probe_path()).expect("re-open reference plugin");
        let f: libloading::Symbol<F> = lib
            .get(b"tutti_test_plugin_activate_refusals\0")
            .expect("activate refusals symbol present");
        f()
    }
}

fn inactive_deactivations() -> u32 {
    type F = unsafe extern "C" fn() -> u32;
    unsafe {
        let lib = libloading::Library::new(probe_path()).expect("re-open reference plugin");
        let f: libloading::Symbol<F> = lib
            .get(b"tutti_test_plugin_inactive_deactivations\0")
            .expect("inactive deactivations symbol present");
        f()
    }
}

fn activate_accepts() -> u32 {
    type F = unsafe extern "C" fn() -> u32;
    unsafe {
        let lib = libloading::Library::new(probe_path()).expect("re-open reference plugin");
        let f: libloading::Symbol<F> = lib
            .get(b"tutti_test_plugin_activate_accepts\0")
            .expect("activate accepts symbol present");
        f()
    }
}

/// `(sample_rate, max_frames)` the probe most recently *accepted*, or `None` if
/// it has accepted nothing since the reset.
fn last_accepted_activation() -> Option<(f64, u32)> {
    type F = unsafe extern "C" fn(*mut u64, *mut u32) -> bool;
    let mut bits = 0u64;
    let mut frames = 0u32;
    unsafe {
        let lib = libloading::Library::new(probe_path()).expect("re-open reference plugin");
        let f: libloading::Symbol<F> = lib
            .get(b"tutti_test_plugin_last_accepted_activation\0")
            .expect("last accepted activation symbol present");
        f(&mut bits, &mut frames).then(|| (f64::from_bits(bits), frames))
    }
}

fn set_state_context_refusal(refuse_save: bool, refuse_load: bool) {
    type F = unsafe extern "C" fn(bool, bool);
    unsafe {
        let lib = libloading::Library::new(probe_path()).expect("re-open reference plugin");
        let f: libloading::Symbol<F> = lib
            .get(b"tutti_test_plugin_set_state_context_refusal\0")
            .expect("state-context refusal symbol present");
        f(refuse_save, refuse_load);
    }
}

fn reset_state_context_refusal() {
    type F = unsafe extern "C" fn();
    unsafe {
        let lib = libloading::Library::new(probe_path()).expect("re-open reference plugin");
        let f: libloading::Symbol<F> = lib
            .get(b"tutti_test_plugin_reset_state_context_refusal\0")
            .expect("reset state-context refusal symbol present");
        f();
    }
}

fn param_reset() {
    type F = unsafe extern "C" fn();
    unsafe {
        let lib = libloading::Library::new(probe_path()).expect("re-open reference plugin");
        let f: libloading::Symbol<F> = lib
            .get(b"tutti_test_plugin_param_reset\0")
            .expect("param reset symbol present");
        f();
    }
}

fn read_param_capture() -> ParamStateCapture {
    type F = unsafe extern "C" fn(*mut ParamStateCapture) -> bool;
    let mut cap = ParamStateCapture::default();
    unsafe {
        let lib = libloading::Library::new(probe_path()).expect("re-open reference plugin");
        let f: libloading::Symbol<F> = lib
            .get(b"tutti_test_plugin_param_capture\0")
            .expect("param capture symbol present");
        assert!(f(&mut cap), "param capture must succeed");
    }
    cap
}

// ---------------------------------------------------------------------------
// `activate` refusal
// ---------------------------------------------------------------------------

/// Deactivates `active` and activates it at `rate`/`frames`, which the plugin
/// is armed to refuse: what the refusal hands back.
fn refused_activation(active: Active<f32>, rate: f64, frames: u32) -> (ClapLoaded, PluginError) {
    active
        .deactivate()
        .activate_at::<f32>(rate, frames)
        .expect_err("the plugin refused the activation; the host must say so")
}

/// A refused activation must be *reported*, not swallowed.
///
/// Mutation (run): `activate_typed` returning `Ok` after a failed
/// `activate_plugin` → the `expect_err` fails.
#[test]
fn refused_sample_rate_change_is_reported() {
    let probe = Probe::acquire();
    let active = probe.activate();

    let target = 96_000.0f64;
    set_activate_refusal(target.to_bits(), 0);
    // Held: the counters live in the loaded image.
    let (_loaded, err) = refused_activation(active, target, BASE_FRAMES);

    assert!(
        matches!(
            err,
            PluginError::Load {
                stage: LoadStage::Activation,
                ..
            }
        ),
        "a plugin declining a configuration is an activation failure: {err:?}"
    );
    assert_eq!(
        activate_refusals(),
        1,
        "the host should have attempted exactly one activation at the refused rate"
    );
}

/// A refused activation hands the plugin back reporting the configuration it
/// already accepted, and the very same instance activates there again.
/// (Putting the previous configuration back automatically is the server
/// bridge's rollback, tested in `tutti-plugin-server`.)
///
/// The oracle is plugin-side: one accepted activation before, one after, the
/// second on the base rate.
///
/// Mutation (run): drop the `self.audio = previous;` restore on the refusal
/// paths of `ClapLoaded::activate_typed` → the handed-back plugin reports the
/// refused rate and the first assertion fails.
#[test]
fn a_refused_activation_leaves_the_accepted_configuration_reported() {
    let probe = Probe::acquire();
    let active = probe.activate();
    assert_eq!(
        activate_accepts(),
        1,
        "one accepted activation at the base rate"
    );

    let target = 96_000.0f64;
    set_activate_refusal(target.to_bits(), 0);
    let (loaded, _) = refused_activation(active, target, BASE_FRAMES);
    set_activate_refusal(ACTIVATE_REFUSE_NONE, 0);

    assert_eq!(
        loaded.sample_rate(),
        BASE_RATE,
        "reporting 96k would tell the caller the plugin is configured for a rate it refused"
    );
    assert_eq!(activate_accepts(), 1, "the refusal activated nothing");

    let active = loaded
        .activate_at::<f32>(BASE_RATE, BASE_FRAMES)
        .map_err(|(_, e)| e)
        .expect("the same instance activates at the accepted configuration");
    assert_eq!(activate_accepts(), 2);
    assert_eq!(last_accepted_activation(), Some((BASE_RATE, BASE_FRAMES)),);
    assert_eq!(active.sample_rate(), BASE_RATE);
}

/// A refused block-size growth leaves the block size the plugin accepted, so
/// `process` does not admit blocks the plugin never agreed to.
///
/// The scratch size is what `process` bounds-checks incoming blocks against
/// (`process_impl` rejects `num_samples > max_frames`). If the host kept
/// `max_frames` at the refused value, `process` would accept a block larger than
/// the plugin's declared ceiling — the plugin reads `frames_count` samples out of
/// buffers it sized for less.
///
/// Mutation (run): as `a_refused_activation_leaves_the_accepted_configuration_reported`
/// — `block_size()` reports 2048.
#[test]
fn a_refused_block_size_growth_leaves_the_accepted_ceiling() {
    let probe = Probe::acquire();
    let active = probe.activate();

    let target = 2048u32;
    set_activate_refusal(ACTIVATE_REFUSE_NONE, target);
    let (loaded, err) = refused_activation(active, BASE_RATE, target);
    set_activate_refusal(ACTIVATE_REFUSE_NONE, 0);

    assert!(matches!(err, PluginError::Load { .. }), "{err:?}");
    assert_eq!(
        loaded.block_size(),
        BASE_FRAMES,
        "the host must report the ceiling the plugin accepted"
    );
}

/// A plugin that refused one configuration still activates and processes at an
/// accepted one: the refusal left no half-state.
#[test]
fn a_plugin_still_processes_after_a_refused_activation() {
    let probe = Probe::acquire();
    let active = probe.activate();

    let target = 96_000.0f64;
    set_activate_refusal(target.to_bits(), 0);
    let (loaded, _) = refused_activation(active, target, BASE_FRAMES);
    set_activate_refusal(ACTIVATE_REFUSE_NONE, 0);
    let mut active = loaded
        .activate_at::<f32>(BASE_RATE, BASE_FRAMES)
        .map_err(|(_, e)| e)
        .expect("activates");

    let frames = 128usize;
    let input_data = vec![vec![0.0f32; frames]; 2];
    let mut output_data = vec![vec![0.0f32; frames]; 2];
    let inputs: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
    let mut outputs: Vec<&mut [f32]> = output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

    let mut buffer = tutti_clap_host::AudioBuffer32 {
        inputs: &inputs,
        outputs: &mut outputs,
        num_samples: frames,
        sample_rate: BASE_RATE,
    };

    let ctx = tutti_clap_host::ClapProcessContext::default();
    active
        .process(&mut buffer, &ctx)
        .expect("a refused-then-activated plugin must process");
}

/// An *accepted* reconfiguration still works, and reaches the plugin.
///
/// The counterpart to the refusal tests: a host that reported an error on every
/// reconfiguration would satisfy them and break the feature.
#[test]
fn accepted_sample_rate_change_reaches_the_plugin() {
    let probe = Probe::acquire();
    let mut active = probe.activate();

    active
        .reconfigure(96_000.0, BASE_FRAMES)
        .expect("the probe accepts 96k when nothing is armed");

    assert_eq!(activate_refusals(), 0);
    assert_eq!(
        last_accepted_activation(),
        Some((96_000.0, BASE_FRAMES)),
        "the new rate must reach the plugin's `activate`"
    );
    assert_eq!(active.sample_rate(), 96_000.0);
}

// ---------------------------------------------------------------------------
// State-context refusal
// ---------------------------------------------------------------------------

/// A refused `state_context.save` must not be answered with a plain
/// `state.save` blob — a substitution that only surfaces later, when someone
/// loads the preset.
///
/// The probe tags every blob with the context it was saved at, and its plain
/// `state.save` still succeeds while the context-aware one is armed. So a
/// fallback is the *only* way this can return `Ok`: `save_calls` counting a
/// plain save is the fingerprint.
#[test]
fn refused_context_save_is_not_silently_downgraded() {
    let probe = Probe::acquire();
    let loaded = probe.load();
    assert!(
        loaded.supports_state_context(),
        "fixture precondition: the probe implements clap.state-context/2, so \
         'absent' and 'present but refusing' are distinguishable"
    );

    set_state_context_refusal(true, false);

    let err = loaded
        .state_with_context(StateContext::ForPreset)
        .expect_err("the plugin refused the preset-context save; the host must not substitute");

    assert!(matches!(err, ClapError::StateError(_)), "{err:?}");

    let cap = read_param_capture();
    assert_eq!(
        cap.save_calls, 0,
        "the host must not fall back to the plain `state.save`: the probe refuses \
         before writing, so a recorded save here is the context-free blob the \
         caller never asked for"
    );
}

/// A refused `state_context.load` must not be retried through the plain
/// `state.load`.
///
/// Worse than the save side: retrying a *rejected* blob through the
/// context-free entry point asks the plugin to swallow bytes it just refused.
/// If it accepts them the caller gets `Ok` on state the plugin said was wrong
/// for this context.
#[test]
fn refused_context_load_is_not_retried_context_free() {
    let probe = Probe::acquire();
    let mut loaded = probe.load();

    // A blob the probe's plain `state.load` accepts, so the fallback path — if
    // taken — succeeds and returns `Ok`. A payload that failed both ways would
    // let a buggy host pass this test for the wrong reason.
    let blob = loaded
        .save_state()
        .expect("the probe's plain save works while nothing is armed");
    param_reset();

    set_state_context_refusal(false, true);

    let err = loaded
        .set_state_with_context(&blob, StateContext::ForPreset)
        .expect_err("the plugin refused the preset-context load; the host must not retry");

    assert!(matches!(err, ClapError::StateError(_)), "{err:?}");

    let cap = read_param_capture();
    assert_eq!(
        cap.load_calls, 0,
        "the host must not retry through the plain `state.load`: the probe refuses \
         before reading, so a recorded load here is the context-free retry"
    );
}

/// Guards the other direction: a host that turned "extension absent" into an
/// error alongside "present and refused" would break every plugin without
/// `clap.state-context/2`.
///
/// With nothing armed, a context save must succeed *and* carry the context tag,
/// proving the host took the context-aware path rather than the fallback. The
/// absent-extension case itself is covered in
/// `clap_params_state_conformance.rs`.
#[test]
fn unrefused_context_save_takes_the_context_aware_path() {
    let probe = Probe::acquire();
    let loaded = probe.load();

    let blob = loaded
        .state_with_context(StateContext::ForPreset)
        .expect("nothing armed: the context-aware save succeeds");
    assert!(!blob.is_empty());

    let cap = read_param_capture();
    assert_eq!(
        cap.save_calls, 1,
        "exactly one save, and it must be the context-aware one"
    );
    assert_ne!(
        cap.last_save_context, 0,
        "context 0 is the probe's tag for the plain `state.save`; a nonzero tag \
         proves the host used `clap.state-context/2`"
    );
}

/// **A plugin whose activation was refused is not deactivated.** An
/// activation the plugin refuses leaves it inactive; dropping it, or
/// activating it again, must not call the plugin's `deactivate`, which CLAP
/// allows only while active.
///
/// Mutation (run): delete the `if !self.flags.active { return; }` guard in
/// `ClapLoaded::deactivate_plugin` and set `flags.active` before the FFI call
/// in `activate_plugin` → the plugin counts a deactivate while inactive → the
/// first count assertion fails. The last assertions also pin that a refused
/// plugin can be brought back once the plugin accepts.
#[test]
fn a_refused_plugin_is_not_deactivated() {
    let probe = Probe::acquire();
    let active = probe.activate();
    let loaded = active.deactivate();
    assert_eq!(
        inactive_deactivations(),
        0,
        "it was active when deactivated"
    );

    set_activate_refusal(96_000f64.to_bits(), BASE_FRAMES);
    let (loaded, reason) = loaded
        .activate_at::<f32>(96_000.0, BASE_FRAMES)
        .expect_err("refused");
    assert!(matches!(reason, PluginError::Load { .. }), "got {reason}");
    assert!(!loaded.is_active());
    let (loaded, reason) = loaded
        .activate_at::<f32>(96_000.0, BASE_FRAMES)
        .expect_err("still refused");
    assert!(matches!(reason, PluginError::Load { .. }), "got {reason}");
    assert_eq!(
        inactive_deactivations(),
        0,
        "an inactive plugin is not deactivated"
    );

    set_activate_refusal(ACTIVATE_REFUSE_NONE, 0);
    let active = loaded
        .activate_at::<f32>(BASE_RATE, BASE_FRAMES)
        .map_err(|(_, e)| e)
        .expect("the plugin accepts again");
    assert!(active.is_active());
    drop(active);
    assert_eq!(inactive_deactivations(), 0);
}
