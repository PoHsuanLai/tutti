//! Debug-only main-thread affinity guard, shared by every plugin-host crate.
//!
//! All native plugin formats (VST3, AU, CLAP) require that calls other than the
//! real-time audio path run on the host's main/UI thread. Violating that is a
//! spec breach that tends to surface as silent state corruption or a crash
//! several blocks later — the hardest class of bug to trace back. This guard
//! turns such a violation into a located panic in debug builds
//! (`debug_assert!`) and compiles to nothing in release.
//!
//! Call [`mark_main_thread`] once, on the UI thread, at host startup. Until it
//! is called the guard is a no-op, so headless tests and tooling that never
//! mark a main thread never false-fire.

use std::sync::OnceLock;
use std::thread::ThreadId;

static MAIN: OnceLock<ThreadId> = OnceLock::new();

/// Record the calling thread as the main/UI thread. Idempotent; the first
/// call wins. Call once at host construction on the UI thread.
pub fn mark_main_thread() {
    let _ = MAIN.set(std::thread::current().id());
}

/// Panic (debug builds only) if the caller is not on the marked main thread.
/// No-op if [`mark_main_thread`] has not been called.
#[inline]
pub fn assert_main_thread() {
    debug_assert!(
        MAIN.get().is_none_or(|m| *m == std::thread::current().id()),
        "plugin main-thread call made off the main thread"
    );
}
