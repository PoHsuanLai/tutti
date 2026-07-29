//! ENUMERATION-HOLE probe: making `get(i)` fail for an `i < count()`.
//!
//! CLAP enumerates ports and parameters as a `count()` / `get(index)` pair, and
//! describes no sparse index space — so a plugin answering `false` below its own
//! `count` is malformed. Plugins do it anyway; what the *host* does next is
//! under test. The failures worth catching are the silent recoveries:
//!
//! - **Audio ports.** A host that closes the gap (`filter_map`) derives a list
//!   one short, with every port past the hole described by its successor's
//!   channel count. That list is the buffer geometry, so channels reach the
//!   wrong port and nothing errors.
//! - **Parameters.** Worse: keyed by id, so nothing visibly shifts. The host
//!   caches each parameter's plain `min`/`max` at activation to denormalize
//!   automation. A dropped parameter is absent from that cache, so its
//!   automation takes the pass-through arm and arrives **un-denormalized** — a
//!   raw `0..1` for a parameter ranged `100..1100`.
//!
//! The probe records the plain value received during `params.flush` so tests can
//! assert that consequence rather than a list length, which a host could fix
//! while leaving the audible bug in place.
//!
//! Both switches are process-global, read live from the vtable callbacks. Set
//! the port hole **before load** (the layout is read once, during load); the
//! parameter hole any time before `activate`, which is when the host enumerates.

use std::sync::atomic::{AtomicU32, Ordering};

/// Sentinel meaning "no hole": every index enumerates normally.
///
/// Not `0`, which is a valid hole index — and the one that distinguishes
/// "truncate" from "fail the load".
pub const HOLE_NONE: u32 = u32::MAX;

/// Index at which `audio_ports_get` reports failure, or [`HOLE_NONE`].
static PORT_HOLE: AtomicU32 = AtomicU32::new(HOLE_NONE);

/// Index at which `params_get_info` reports failure, or [`HOLE_NONE`].
static PARAM_HOLE: AtomicU32 = AtomicU32::new(HOLE_NONE);

/// Whether `audio_ports_get(index)` should report failure.
pub(crate) fn port_hole_at(index: u32) -> bool {
    PORT_HOLE.load(Ordering::SeqCst) == index
}

/// Whether `params_get_info(index)` should report failure.
pub(crate) fn param_hole_at(index: u32) -> bool {
    PARAM_HOLE.load(Ordering::SeqCst) == index
}

/// Make `audio_ports_get` return `false` for `index`, while
/// `audio_ports_count` keeps reporting the full count.
///
/// Pass [`HOLE_NONE`] to clear. Call **before** the host loads the plugin: the
/// port layout is read once during load.
///
/// # Safety
/// Safe to call; `extern "C"` only so the test can reach it across `dlopen`.
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_set_port_hole(index: u32) {
    PORT_HOLE.store(index, Ordering::SeqCst);
}

/// Make `params_get_info` return `false` for `index`, while `params_count`
/// keeps reporting the full count.
///
/// Pass [`HOLE_NONE`] to clear. Call before the host enumerates parameters —
/// it does so during `activate`, to build its denormalization cache.
///
/// # Safety
/// Safe to call; `extern "C"` only so the test can reach it across `dlopen`.
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_set_param_hole(index: u32) {
    PARAM_HOLE.store(index, Ordering::SeqCst);
}

/// Clear both holes. Tests call this so one test's switch cannot leak into the
/// next — every switch here is a process-global shared by the whole binary.
///
/// # Safety
/// Safe to call; `extern "C"` only so the test can reach it across `dlopen`.
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_clear_holes() {
    PORT_HOLE.store(HOLE_NONE, Ordering::SeqCst);
    PARAM_HOLE.store(HOLE_NONE, Ordering::SeqCst);
}
