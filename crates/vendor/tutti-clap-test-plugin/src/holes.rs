//! ENUMERATION-HOLE probe: making `get(i)` fail for an `i < count()`.
//!
//! CLAP enumerates audio ports and parameters as a `count()` / `get(index)`
//! pair. `audio-ports.h` calls `count` the "Number of ports" and `get`
//! "Returns true on success"; `params.h` says `count` "Returns the number of
//! parameters" and `get_info` "Returns true on success". Neither describes a
//! sparse index space, so a plugin that answers `false` for an index below its
//! own `count` is malformed — but plugins do it, and what the *host* does next
//! is the thing under test.
//!
//! The interesting failures are not that the host notices. They are the two
//! silent recoveries:
//!
//! - **Audio ports.** If the host closes the gap (`filter_map`), the port list
//!   it derives is one entry short and every port past the hole is described by
//!   its *successor's* channel count. That list is positional — it is the whole
//!   description of the buffer geometry — so the host then hands the plugin
//!   channels belonging to the wrong port. Nothing errors.
//!
//! - **Parameters.** Worse, because parameters are keyed by id and so nothing
//!   visibly shifts. The host caches each parameter's plain `min`/`max` at
//!   activation to denormalize incoming automation (host automation is authored
//!   `0..1`; CLAP events carry the plugin's plain value). A parameter the hole
//!   dropped is simply absent from that cache, so its automation takes the
//!   pass-through arm and reaches the plugin **un-denormalized** — a raw `0..1`
//!   delivered to a parameter whose range is `100..1100`.
//!
//! That second one is why this module exists at all rather than the tests just
//! asserting a list length: the list length is a proxy, and a host could fix
//! the length while leaving the audible bug in place. The probe records the
//! plain value it actually received during `params.flush`, so the test can
//! assert the *consequence*.
//!
//! Both switches are process-global and read live from the vtable callbacks,
//! following the pattern the rest of this crate uses (`PORT_LAYOUT`,
//! `RENDER_MODE`, the `threading` command channel). The port hole must be set
//! **before the host loads the plugin** — the host reads the port layout once,
//! during load. The parameter hole may be set any time before the host
//! enumerates parameters, which it does during `activate`.

use std::sync::atomic::{AtomicU32, Ordering};

/// Sentinel meaning "no hole": every index enumerates normally.
///
/// `u32::MAX` rather than a signed -1 because these cross the `dlopen` seam as
/// bare `u32`, and rather than `0` because 0 is a perfectly good index to
/// punch a hole at — a test that puts the hole first is exactly the one that
/// distinguishes "truncate" from "fail the load".
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
