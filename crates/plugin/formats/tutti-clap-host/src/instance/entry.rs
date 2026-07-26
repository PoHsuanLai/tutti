//! CLAP entry init/deinit lifecycle: the once-per-library registry and its
//! RAII guard.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

/// Global registry for CLAP entry init/deinit lifecycle.
///
/// The CLAP spec requires `clap_entry.init()` to be called once when a library
/// is first loaded and `clap_entry.deinit()` once when it's finally unloaded.
/// Many plugins do not tolerate repeated init/deinit cycles within the same
/// process — their global state becomes corrupted. This registry ensures
/// `init()` is called exactly once per library path *per loaded image*, for as
/// long as that image stays mapped. `deinit()` is intentionally NOT called,
/// matching real-world DAW behavior where plugins run in subprocesses that exit
/// cleanly.
///
/// # Why the value is a `Weak`, not a `bool` (H6)
/// A plain "initialized: bool" entry outlives the image it describes. `probe()`
/// dlopens a library, registers the path, reads the descriptor, then **drops
/// the library** — `dlclose` unmaps the image and its `init` side effects go
/// with it, but the registry still said "initialized". The very next `load()`
/// of that same path re-dlopens a *fresh* image and skipped `init` entirely,
/// leaving the plugin uninitialized. That is the standard scan-then-load flow,
/// so it was the common case, not an edge case.
///
/// Tying the entry to a `Weak<LiveEntry>` fixes it structurally: the registry
/// holds no ownership, and every [`EntryGuard`] holds a strong reference. When
/// the last guard for a path drops (the image is about to be unloaded), the
/// `Weak` can no longer be upgraded, so the next acquire re-runs `init` on the
/// new image. While *any* guard is alive the upgrade succeeds and `init` is
/// correctly skipped — preserving the once-per-image guarantee that plugins
/// with fragile global state depend on.
static ENTRY_REGISTRY: Mutex<Option<HashMap<PathBuf, Weak<LiveEntry>>>> = Mutex::new(None);

/// Liveness token for one initialized CLAP entry. Its existence is the proof
/// that the corresponding library image is still mapped and still initialized.
pub(crate) struct LiveEntry {
    _path: PathBuf,
}

/// RAII guard for CLAP entry lifetime. Does not call deinit on drop — but its
/// drop does release the registry's claim that the path is initialized, so a
/// later load of a re-dlopen'd image runs `init` again (H6).
pub(crate) struct EntryGuard {
    _live: Arc<LiveEntry>,
}

/// Register a CLAP entry for the given path.
///
/// Calls `init_fn` on the first load of a given library, and again after every
/// previous load of that path has been dropped (its image unloaded). While a
/// load is outstanding, further acquires share the existing initialization.
pub(crate) fn entry_registry_acquire(
    path: &Path,
    init_fn: unsafe extern "C" fn(*const i8) -> bool,
    path_cstr: &std::ffi::CString,
) -> std::result::Result<EntryGuard, String> {
    let mut registry = ENTRY_REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    let map = registry.get_or_insert_with(HashMap::new);

    // An entry whose `Weak` still upgrades names a live, already-initialized
    // image: share it. A stale entry (all guards dropped → image unloaded)
    // upgrades to `None` and falls through to a fresh `init`.
    if let Some(live) = map.get(path).and_then(Weak::upgrade) {
        return Ok(EntryGuard { _live: live });
    }

    if !unsafe { init_fn(path_cstr.as_ptr()) } {
        return Err("Entry init failed".to_string());
    }

    let live = Arc::new(LiveEntry {
        _path: path.to_path_buf(),
    });
    map.insert(path.to_path_buf(), Arc::downgrade(&live));
    Ok(EntryGuard { _live: live })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    // One counter per test. `init_fn` is a bare `extern "C" fn` with no context
    // pointer, so each test needs its own static + wrapper; a single shared
    // counter would race under the default parallel test runner.
    static REINIT_CALLS: AtomicU32 = AtomicU32::new(0);
    static SHARED_CALLS: AtomicU32 = AtomicU32::new(0);
    static AFTER_FAILURE_CALLS: AtomicU32 = AtomicU32::new(0);

    unsafe extern "C" fn reinit_counting_init(_path: *const i8) -> bool {
        REINIT_CALLS.fetch_add(1, Ordering::SeqCst);
        true
    }

    unsafe extern "C" fn shared_counting_init(_path: *const i8) -> bool {
        SHARED_CALLS.fetch_add(1, Ordering::SeqCst);
        true
    }

    unsafe extern "C" fn after_failure_counting_init(_path: *const i8) -> bool {
        AFTER_FAILURE_CALLS.fetch_add(1, Ordering::SeqCst);
        true
    }

    unsafe extern "C" fn failing_init(_path: *const i8) -> bool {
        false
    }

    /// H6 regression: `probe()` registers a path then drops the library, so the
    /// registry entry must NOT outlive the guard. A later `load()` of the same
    /// path re-dlopens a fresh image and MUST run `init` again — the old
    /// `HashMap<PathBuf, bool>` marked the path initialized forever and skipped
    /// it, leaving the second image uninitialized.
    #[test]
    fn init_runs_again_after_every_guard_for_a_path_is_dropped() {
        let path = Path::new("/test/h6-reinit-after-unload.clap");
        let cstr = std::ffi::CString::new("/test/h6-reinit-after-unload.clap").unwrap();

        // Stands in for `probe()`: acquire, then drop (library unloaded).
        let guard = entry_registry_acquire(path, reinit_counting_init, &cstr).unwrap();
        assert_eq!(REINIT_CALLS.load(Ordering::SeqCst), 1);
        drop(guard);

        // Stands in for the `load()` that follows the scan.
        let guard = entry_registry_acquire(path, reinit_counting_init, &cstr).unwrap();
        assert_eq!(
            REINIT_CALLS.load(Ordering::SeqCst),
            2,
            "a re-dlopen'd image must be init'd again, not skipped"
        );
        drop(guard);
    }

    /// The once-per-image guarantee that plugins with fragile global state rely
    /// on still holds: overlapping acquires share one `init`.
    #[test]
    fn init_runs_once_while_a_guard_is_still_alive() {
        let path = Path::new("/test/h6-shared-while-live.clap");
        let cstr = std::ffi::CString::new("/test/h6-shared-while-live.clap").unwrap();

        let first = entry_registry_acquire(path, shared_counting_init, &cstr).unwrap();
        let second = entry_registry_acquire(path, shared_counting_init, &cstr).unwrap();
        assert_eq!(SHARED_CALLS.load(Ordering::SeqCst), 1);

        // Dropping one of two guards must not release the claim.
        drop(first);
        let third = entry_registry_acquire(path, shared_counting_init, &cstr).unwrap();
        assert_eq!(SHARED_CALLS.load(Ordering::SeqCst), 1);
        drop(second);
        drop(third);
    }

    /// A failed `init` must not register the path, or the next acquire would
    /// skip init and hand back a guard for an uninitialized entry.
    #[test]
    fn failed_init_is_not_registered() {
        let path = Path::new("/test/h6-failed-init.clap");
        let cstr = std::ffi::CString::new("/test/h6-failed-init.clap").unwrap();

        assert!(entry_registry_acquire(path, failing_init, &cstr).is_err());

        let guard = entry_registry_acquire(path, after_failure_counting_init, &cstr).unwrap();
        assert_eq!(
            AFTER_FAILURE_CALLS.load(Ordering::SeqCst),
            1,
            "a failed init must leave the path unregistered"
        );
        drop(guard);
    }
}
