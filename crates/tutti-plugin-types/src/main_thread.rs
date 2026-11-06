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

/// Marks the calling thread as the host's main/UI thread.
///
/// Enables the debug-only affinity checks of [`assert_main_thread`]. Call once,
/// on the UI thread, at host startup; later calls are ignored (the first call
/// wins). Until it is called the checks are a no-op.
pub fn mark_main_thread() {
    let _ = MAIN.set(std::thread::current().id());
}

/// Asserts that the caller is on the thread [`mark_main_thread`] marked.
///
/// All native plugin formats require non-audio calls (editor, state,
/// parameter enumeration) on the host's main thread, and a violation tends to
/// surface as corruption or a crash much later. Compiles to nothing in release
/// builds, and is a no-op if no main thread was marked.
///
/// # Panics
///
/// In debug builds, panics if a main thread was marked and the caller is on a
/// different thread.
#[inline]
pub fn assert_main_thread() {
    debug_assert!(
        MAIN.get().is_none_or(|m| *m == std::thread::current().id()),
        "plugin main-thread call made off the main thread"
    );
}
