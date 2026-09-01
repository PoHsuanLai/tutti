//! REFUSAL probe: make the plugin say **no** where CLAP entitles it to, and
//! record what the host did next.
//!
//! Two switches, one theme. `clap_plugin.activate` and
//! `clap_plugin_state_context.save`/`.load` all return `bool`, and in all three
//! `false` means *"no"* — not *"not applicable"*. A host that conflates the two
//! either loses track of the plugin's state or silently substitutes something
//! the caller did not ask for. A plugin that always says yes cannot expose
//! either mistake, so the probe needs a way to say no on demand.
//!
//! Separate module so each thing the probe models is an oracle that cannot
//! perturb the others; `lib.rs` and `params_state.rs` hold only the call-site
//! hooks.
//!
//! **Activation.** A plugin may reject a sample rate or block size it cannot
//! run. Counters alone would only show the host *stopped*; the question is
//! whether it **recovered**. [`crate::refusal::tutti_test_plugin_last_accepted_activation`] plus
//! [`crate::refusal::tutti_test_plugin_activate_accepts`] tell those apart — one accept with
//! stale values means the host abandoned the instance, two accepts landing on
//! the previous configuration mean it rolled back and re-activated.
//!
//! **State context.** Only the context-aware entry point refuses, leaving plain
//! `state.save` working. That is what makes substitution observable: a host that
//! falls through returns `Ok` carrying bytes the probe tagged with context 0 —
//! a blob saved for a context the caller never asked for.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// Sample-rate sentinel meaning "refuse nothing".
///
/// Not `0.0` or `-1.0`: those are values a *buggy host* could plausibly pass,
/// and a sentinel that collides with a host bug would mask it. `u64::MAX` is
/// not the bit pattern of any rate a host can ask for.
pub const ACTIVATE_REFUSE_NONE: u64 = u64::MAX;

/// Sample rate `activate` refuses, as `f64::to_bits`, or
/// [`ACTIVATE_REFUSE_NONE`].
///
/// Bits in an `AtomicU64` rather than a `Mutex<f64>` because `activate` is a
/// `[main-thread]` call the host makes while the test holds nothing the plugin
/// can see, and because the match must be **bit-exact**: the host round-trips
/// this value through its own config without arithmetic, so matching
/// approximately would hide a host that perturbed it.
static REFUSE_RATE: AtomicU64 = AtomicU64::new(ACTIVATE_REFUSE_NONE);

/// `max_frames` value `activate` refuses, or 0 for "refuse nothing".
///
/// 0 is a safe sentinel here in a way it is not for the rate: `max_frames` is
/// the ceiling on `frames_count`, so a host activating at 0 could never legally
/// call `process`. No real host asks for it.
static REFUSE_FRAMES: AtomicU32 = AtomicU32::new(0);

/// Refused-`activate` count since the last [`reset`].
static REFUSALS: AtomicU32 = AtomicU32::new(0);

/// Accepted-`activate` count since the last [`reset`]. Distinguishes "rolled
/// back" (two accepts — the original plus the rollback) from "abandoned" (one).
static ACCEPTS: AtomicU32 = AtomicU32::new(0);

/// `(sample_rate bits, max_frames)` of the most recent **accepted** `activate`.
static LAST_RATE: AtomicU64 = AtomicU64::new(0);
static LAST_FRAMES: AtomicU32 = AtomicU32::new(0);

/// The `activate` hook. Returns what the plugin should report, recording the
/// call either way. Called from `plugin_activate` in `lib.rs`.
///
/// `active` is the *instance's* flag, not a global — see `PluginState::active`
/// in `lib.rs` for why that distinction matters here.
pub(crate) fn on_activate(active: &AtomicBool, sample_rate: f64, max_frames: u32) -> bool {
    let refuse_rate = REFUSE_RATE.load(Ordering::SeqCst);
    let refuse_frames = REFUSE_FRAMES.load(Ordering::SeqCst);

    // Either criterion matches on its own, so a test can target `set_sample_rate`
    // or `set_max_block_size` without having to predict the argument it does not
    // control.
    let refuse = (refuse_rate != ACTIVATE_REFUSE_NONE && sample_rate.to_bits() == refuse_rate)
        || (refuse_frames != 0 && max_frames == refuse_frames);

    if refuse {
        REFUSALS.fetch_add(1, Ordering::SeqCst);
        // A plugin that returns `false` from `activate` is not active. The host
        // called `deactivate` on the way in, so the flag is already false; make
        // it unconditional anyway rather than rely on that ordering.
        active.store(false, Ordering::SeqCst);
        return false;
    }

    LAST_RATE.store(sample_rate.to_bits(), Ordering::SeqCst);
    LAST_FRAMES.store(max_frames, Ordering::SeqCst);
    ACCEPTS.fetch_add(1, Ordering::SeqCst);
    active.store(true, Ordering::SeqCst);
    true
}

/// Make `activate` return `false` for a configuration.
///
/// `refuse_rate_bits` is a sample rate as `f64::to_bits` or
/// [`ACTIVATE_REFUSE_NONE`]; `refuse_frames` is a `max_frames` value or 0.
///
/// # Safety
/// Safe to call; `extern "C"` only so the test can reach it across `dlopen`.
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_set_activate_refusal(
    refuse_rate_bits: u64,
    refuse_frames: u32,
) {
    REFUSE_RATE.store(refuse_rate_bits, Ordering::SeqCst);
    REFUSE_FRAMES.store(refuse_frames, Ordering::SeqCst);
}

/// Clear the refusal switch and every activation counter.
///
/// Tests call this before driving the host, because these are process-globals
/// shared by every test in the binary — without it an assertion could be
/// satisfied by a *previous* test's activation.
///
/// # Safety
/// Safe to call; `extern "C"` only so the test can reach it across `dlopen`.
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_reset_activation() {
    REFUSE_RATE.store(ACTIVATE_REFUSE_NONE, Ordering::SeqCst);
    REFUSE_FRAMES.store(0, Ordering::SeqCst);
    REFUSALS.store(0, Ordering::SeqCst);
    ACCEPTS.store(0, Ordering::SeqCst);
    LAST_RATE.store(0, Ordering::SeqCst);
    LAST_FRAMES.store(0, Ordering::SeqCst);
    // Not `ACTIVE`: that mirrors the plugin's real state, which a test cannot
    // wish away. Clearing it here would make `start_processing` reject a plugin
    // an earlier fixture legitimately activated.
}

/// Refused-`activate` count since the last [`tutti_test_plugin_reset_activation`].
///
/// # Safety
/// Safe to call; `extern "C"` only so the test can reach it across `dlopen`.
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_activate_refusals() -> u32 {
    REFUSALS.load(Ordering::SeqCst)
}

/// Accepted-`activate` count since the last [`tutti_test_plugin_reset_activation`].
///
/// # Safety
/// Safe to call; `extern "C"` only so the test can reach it across `dlopen`.
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_activate_accepts() -> u32 {
    ACCEPTS.load(Ordering::SeqCst)
}

/// `(sample_rate bits, max_frames)` of the most recently **accepted**
/// `activate`. Returns false and writes nothing if the plugin has accepted no
/// activation since the last reset.
///
/// # Safety
/// `rate_bits_out` / `frames_out` must be null or point to valid, writable
/// `u64` / `u32`.
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_last_accepted_activation(
    rate_bits_out: *mut u64,
    frames_out: *mut u32,
) -> bool {
    if ACCEPTS.load(Ordering::SeqCst) == 0 {
        return false;
    }
    if !rate_bits_out.is_null() {
        *rate_bits_out = LAST_RATE.load(Ordering::SeqCst);
    }
    if !frames_out.is_null() {
        *frames_out = LAST_FRAMES.load(Ordering::SeqCst);
    }
    true
}

// ---------------------------------------------------------------------------
// State-context refusal.
// ---------------------------------------------------------------------------

/// Whether `clap_plugin_state_context.save` should return `false`.
static REFUSE_CONTEXT_SAVE: AtomicBool = AtomicBool::new(false);

/// Whether `clap_plugin_state_context.load` should return `false`.
static REFUSE_CONTEXT_LOAD: AtomicBool = AtomicBool::new(false);

/// Refusal hook for `state_context_save`. Called from `params_state.rs`.
pub(crate) fn refuse_context_save() -> bool {
    REFUSE_CONTEXT_SAVE.load(Ordering::SeqCst)
}

/// Refusal hook for `state_context_load`. Called from `params_state.rs`.
pub(crate) fn refuse_context_load() -> bool {
    REFUSE_CONTEXT_LOAD.load(Ordering::SeqCst)
}

/// Make the `clap.state-context/2` entry points return `false` while leaving
/// the plain `clap.state` ones working.
///
/// That asymmetry is the point. The extension stays *present* — so "absent,
/// fall back" and "present and refused" are distinguishable — and the plain
/// path still succeeds, so a host that wrongly falls back to it returns `Ok`
/// rather than an error that would mask the bug as a mere propagation.
///
/// # Safety
/// Safe to call; `extern "C"` only so the test can reach it across `dlopen`.
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_set_state_context_refusal(
    refuse_save: bool,
    refuse_load: bool,
) {
    REFUSE_CONTEXT_SAVE.store(refuse_save, Ordering::SeqCst);
    REFUSE_CONTEXT_LOAD.store(refuse_load, Ordering::SeqCst);
}

/// Clear both state-context refusal switches.
///
/// # Safety
/// Safe to call; `extern "C"` only so the test can reach it across `dlopen`.
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_reset_state_context_refusal() {
    REFUSE_CONTEXT_SAVE.store(false, Ordering::SeqCst);
    REFUSE_CONTEXT_LOAD.store(false, Ordering::SeqCst);
}
