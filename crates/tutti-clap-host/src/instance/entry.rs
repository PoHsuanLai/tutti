//! CLAP entry init/deinit lifecycle: the once-per-library registry and its
//! RAII guard.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Global registry for CLAP entry init/deinit lifecycle.
///
/// The CLAP spec requires `clap_entry.init()` to be called once when a library
/// is first loaded and `clap_entry.deinit()` once when it's finally unloaded.
/// Many plugins do not tolerate repeated init/deinit cycles within the same
/// process — their global state becomes corrupted. This registry ensures
/// `init()` is called exactly once per library path for the lifetime of the
/// process. `deinit()` is intentionally NOT called, matching real-world DAW
/// behavior where plugins run in subprocesses that exit cleanly.
static ENTRY_REGISTRY: Mutex<Option<HashMap<PathBuf, bool>>> = Mutex::new(None);

/// RAII guard for CLAP entry lifetime. Does not call deinit on drop — the
/// entry stays initialized for the lifetime of the process.
pub(crate) struct EntryGuard {
    _path: PathBuf,
}

/// Register a CLAP entry for the given path.
/// Calls `init_fn` only on the first load of a given library. Subsequent
/// loads of the same library skip init (the entry is already initialized).
pub(crate) fn entry_registry_acquire(
    path: &Path,
    init_fn: unsafe extern "C" fn(*const i8) -> bool,
    path_cstr: &std::ffi::CString,
) -> std::result::Result<EntryGuard, String> {
    let mut registry = ENTRY_REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    let map = registry.get_or_insert_with(HashMap::new);

    if !map.contains_key(path) {
        if !unsafe { init_fn(path_cstr.as_ptr()) } {
            return Err("Entry init failed".to_string());
        }
        map.insert(path.to_path_buf(), true);
    }

    Ok(EntryGuard {
        _path: path.to_path_buf(),
    })
}
