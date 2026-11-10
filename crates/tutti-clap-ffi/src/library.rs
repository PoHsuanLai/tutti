//! Loading a CLAP bundle: the library, the entry's once-per-image
//! initialisation, the factory and its descriptors.

use std::collections::HashMap;
use std::ffi::{CString, c_char};
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use clap_sys::entry::clap_plugin_entry;
use clap_sys::factory::plugin_factory::{CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory};
use clap_sys::plugin::clap_plugin_descriptor;
use clap_sys::version::{CLAP_VERSION_MAJOR, clap_version, clap_version_is_compatible};

use crate::guard::lossy;
use crate::host::Host;
use crate::plugin::Plugin;
use crate::roles::ThreadRoles;

/// Which step of loading failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Opening the library or reading `clap_entry`.
    Opening,
    /// Getting the plugin factory and its descriptors.
    Factory,
    /// Creating the instance.
    Instantiation,
    /// The plugin's own `init`.
    Initialization,
}

/// A load that failed: which step, and what went wrong at it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LoadError {
    /// The library could not be opened; the loader's message.
    Library(String),
    /// The library has no `clap_entry` symbol; the loader's message.
    NoEntrySymbol(String),
    /// The path is not a C string; the reason.
    InvalidPath(String),
    /// The entry's `init` answered `false`.
    EntryInit,
    /// An entry the CLAP structs define is absent (a null function pointer).
    MissingFunction {
        /// The step that needed it.
        stage: Stage,
        /// Its name, as `clap.h` spells it.
        name: &'static str,
    },
    /// The entry or a descriptor declares a CLAP version this host cannot read.
    Version {
        /// What declared it.
        what: &'static str,
        /// The step that read it.
        stage: Stage,
        /// Its major version.
        major: u32,
        /// Its minor version.
        minor: u32,
        /// Its revision.
        revision: u32,
    },
    /// The entry returned no plugin factory.
    NoFactory,
    /// The factory advertises no plugins.
    EmptyFactory,
    /// The factory advertises plugins but returned no descriptor for any.
    NoDescriptors {
        /// How many it advertised.
        count: u32,
    },
    /// No descriptor has the id asked for.
    UnknownId {
        /// The id asked for.
        wanted: String,
        /// The ids the bundle holds (`<null id>` for a descriptor without one).
        available: Vec<String>,
    },
    /// The plugin id is not a C string; the reason.
    InvalidPluginId(String),
    /// The factory returned no instance.
    CreateFailed,
    /// The plugin's own `init` answered `false`.
    PluginInitFailed,
}

impl LoadError {
    /// The step that failed.
    #[must_use]
    pub fn stage(&self) -> Stage {
        match self {
            Self::Library(_) | Self::NoEntrySymbol(_) | Self::InvalidPath(_) | Self::EntryInit => {
                Stage::Opening
            }
            Self::MissingFunction { stage, .. } | Self::Version { stage, .. } => *stage,
            Self::NoFactory
            | Self::EmptyFactory
            | Self::NoDescriptors { .. }
            | Self::UnknownId { .. } => Stage::Factory,
            Self::InvalidPluginId(_) | Self::CreateFailed => Stage::Instantiation,
            Self::PluginInitFailed => Stage::Initialization,
        }
    }

    /// Why, without the step.
    #[must_use]
    pub fn reason(&self) -> String {
        match self {
            Self::Library(e) => format!("Failed to load library: {e}"),
            Self::NoEntrySymbol(e) => format!("No clap_entry symbol: {e}"),
            Self::InvalidPath(e) => format!("Invalid path: {e}"),
            Self::EntryInit => "Entry init failed".to_string(),
            Self::MissingFunction { name, .. } => format!("No {name} function"),
            Self::Version {
                what,
                major,
                minor,
                revision,
                ..
            } => format!(
                "{what} declares CLAP {major}.{minor}.{revision}, which this host cannot read \
                 its structs against: it is built for major {CLAP_VERSION_MAJOR} \
                 (version.h sets the floor at major >= 1 — 0.x is the development \
                 stage, with no stable ABI)"
            ),
            Self::NoFactory => "No plugin factory".to_string(),
            Self::EmptyFactory => "No plugins in factory".to_string(),
            Self::NoDescriptors { count } => {
                format!("Factory advertises {count} plugins but returned no descriptor")
            }
            Self::UnknownId { wanted, available } => format!(
                "No plugin with id '{wanted}' in this bundle; it contains: {}",
                available.join(", ")
            ),
            Self::InvalidPluginId(e) => format!("Invalid plugin ID: {e}"),
            Self::CreateFailed => "Failed to create plugin instance".to_string(),
            Self::PluginInitFailed => "Plugin init failed".to_string(),
        }
    }
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.stage(), self.reason())
    }
}

impl std::error::Error for LoadError {}

/// What a descriptor declares, as owned strings.
#[derive(Debug, Clone)]
pub struct DescriptorInfo {
    /// Reverse-DNS plugin id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Vendor, empty when omitted.
    pub vendor: String,
    /// Version string.
    pub version: String,
    /// URL, empty when omitted.
    pub url: String,
    /// Description, empty when omitted.
    pub description: String,
    /// Feature tags.
    pub features: Vec<String>,
    version_of_clap: clap_version,
}

/// Copies a fixed C char field up to its first nul (or its end), lossily.
#[must_use]
pub fn chars_to_string(field: &[c_char]) -> String {
    let bytes: Vec<u8> = field
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Global registry for CLAP entry init/deinit lifecycle.
///
/// The CLAP spec requires `clap_entry.init()` once when a library is first
/// loaded and `clap_entry.deinit()` once when it is finally unloaded. Many
/// plugins do not tolerate repeated init/deinit cycles within one process.
/// This registry calls `init()` exactly once per library path *per loaded
/// image*, for as long as that image stays mapped. `deinit()` is not called,
/// matching real-world DAW behaviour where plugins run in subprocesses that
/// exit cleanly.
///
/// The value is a `Weak`, not a `bool`: `probe` dlopens a library, registers
/// the path, reads the descriptor, then drops the library, so a plain flag
/// would outlive the image it describes and the next load would skip `init`
/// on a fresh image. Every [`EntryGuard`] holds a strong reference; when the
/// last one drops (the image is about to be unloaded) the `Weak` stops
/// upgrading and the next acquire re-runs `init`.
static ENTRY_REGISTRY: Mutex<Option<HashMap<PathBuf, Weak<LiveEntry>>>> = Mutex::new(None);

type Registry = Option<HashMap<PathBuf, Weak<LiveEntry>>>;

/// The registry, locked. A poisoned lock is taken as is: the map is only
/// ever inserted into, so a panicking holder left it consistent.
fn registry() -> MutexGuard<'static, Registry> {
    ENTRY_REGISTRY.lock().unwrap_or_else(|e| e.into_inner())
}

/// Liveness token of one initialised entry: its existence proves the image
/// is still mapped and initialised.
struct LiveEntry;

/// RAII guard for entry lifetime. Does not deinit on drop, but releases the
/// registry's claim that the path is initialised.
struct EntryGuard {
    _live: Arc<LiveEntry>,
}

type InitFn = unsafe extern "C" fn(*const c_char) -> bool;

#[cfg(test)]
fn entry_registry_acquire(
    path: &Path,
    init_fn: InitFn,
    path_cstr: &CString,
) -> Result<EntryGuard, LoadError> {
    acquire_in(&mut registry(), path, init_fn, path_cstr)
}

/// The entry claim for `path` on a registry the caller already holds locked.
///
/// `Bundle::open` holds the lock from before it maps the image until the
/// entry is acquired, and `Bundle`'s drop holds it from before the image is
/// unmapped until the guard is released. The two cannot interleave, so a
/// `Weak` that upgrades always names the image the caller just mapped (still
/// mapped by the other holder, which it shares), and one that does not names
/// no image at all: `init` never runs twice on one image, nor is skipped on a
/// fresh one.
fn acquire_in(
    registry: &mut Registry,
    path: &Path,
    init_fn: InitFn,
    path_cstr: &CString,
) -> Result<EntryGuard, LoadError> {
    let map = registry.get_or_insert_with(HashMap::new);
    if let Some(live) = map.get(path).and_then(Weak::upgrade) {
        return Ok(EntryGuard { _live: live });
    }
    // SAFETY: `init_fn` is the loaded image's `clap_entry.init`, called with a
    // nul-terminated path that outlives the call, once per image (above).
    if !unsafe { init_fn(path_cstr.as_ptr()) } {
        return Err(LoadError::EntryInit);
    }
    let live = Arc::new(LiveEntry);
    map.insert(path.to_path_buf(), Arc::downgrade(&live));
    Ok(EntryGuard { _live: live })
}

/// An opened CLAP bundle: its library, initialised entry and factory.
///
/// Dropping it unloads the library, then releases the entry claim, both under
/// the registry lock (see `acquire_in`).
pub struct Bundle {
    // `ManuallyDrop` so `Drop` can close the image and release the claim in
    // one hold of the registry lock.
    _entry: ManuallyDrop<EntryGuard>,
    library: ManuallyDrop<libloading::Library>,
    factory: *const clap_plugin_factory,
    descriptors: Vec<DescriptorInfo>,
}

// SAFETY: `factory` points into the image `library` keeps mapped and
// are read-only data CLAP defines as valid for the library's lifetime and
// thread-safe to read (`factory.h`: `[thread-safe]` entries); the rest is
// `Send` already. Moving the bundle to another thread moves no thread-affine
// state.
unsafe impl Send for Bundle {}

impl std::fmt::Debug for Bundle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bundle")
            .field("library", &self.library)
            .field("descriptors", &self.descriptors)
            .finish_non_exhaustive()
    }
}

impl Drop for Bundle {
    fn drop(&mut self) {
        let _registry = registry();
        // SAFETY: each is dropped once, here, and never used again. The
        // library goes first (the image unmaps), then the claim that the image
        // is initialised, so no opener sees the claim outlive its image.
        unsafe {
            ManuallyDrop::drop(&mut self.library);
            ManuallyDrop::drop(&mut self._entry);
        }
    }
}

fn check_version(version: clap_version, what: &'static str, stage: Stage) -> Result<(), LoadError> {
    if clap_version_is_compatible(version) && version.major <= CLAP_VERSION_MAJOR {
        return Ok(());
    }
    Err(LoadError::Version {
        what,
        stage,
        major: version.major,
        minor: version.minor,
        revision: version.revision,
    })
}

impl Bundle {
    /// Opens `library_path` (or `bundle_path` itself), initialises its entry
    /// and reads every descriptor its factory advertises.
    ///
    /// # Errors
    /// A [`LoadError`] naming the failed step: the library, the entry, the
    /// factory or its descriptors.
    pub fn open(bundle_path: &Path, library_path: Option<&Path>) -> Result<Self, LoadError> {
        let load_path = library_path.unwrap_or(bundle_path);
        // Held until the entry is acquired: see `acquire_in`.
        let mut registry = registry();
        // SAFETY: loading a plugin library runs its initialisers, which the
        // host accepts by loading plugins at all; the caller named the path.
        let library = unsafe { libloading::Library::new(load_path) }
            .map_err(|e| LoadError::Library(e.to_string()))?;

        // SAFETY: `clap_entry` is the symbol CLAP defines, a `clap_plugin_entry`
        // static of the image, valid while `library` is mapped.
        let entry: &clap_plugin_entry = unsafe {
            let sym = library
                .get::<*const clap_plugin_entry>(b"clap_entry\0")
                .map_err(|e| LoadError::NoEntrySymbol(e.to_string()))?;
            &**sym
        };
        check_version(entry.clap_version, "clap_entry", Stage::Opening)?;

        let init_fn = entry.init.ok_or(LoadError::MissingFunction {
            stage: Stage::Opening,
            name: "init",
        })?;
        let path_cstr = CString::new(bundle_path.to_string_lossy().as_ref())
            .map_err(|e| LoadError::InvalidPath(e.to_string()))?;
        let entry_guard = acquire_in(&mut registry, bundle_path, init_fn, &path_cstr)?;

        let get_factory = entry.get_factory.ok_or(LoadError::MissingFunction {
            stage: Stage::Factory,
            name: "get_factory",
        })?;
        // SAFETY: `get_factory` is the entry's, called after `init` with the
        // factory id CLAP defines.
        let factory =
            unsafe { get_factory(CLAP_PLUGIN_FACTORY_ID.as_ptr()) }.cast::<clap_plugin_factory>();
        if factory.is_null() {
            return Err(LoadError::NoFactory);
        }
        // SAFETY: non-null factory the image returned, valid while mapped.
        let factory_ref = unsafe { &*factory };
        let count_fn = factory_ref
            .get_plugin_count
            .ok_or(LoadError::MissingFunction {
                stage: Stage::Factory,
                name: "get_plugin_count",
            })?;
        // SAFETY: the factory's own entry, called with the factory.
        let count = unsafe { count_fn(factory) };
        if count == 0 {
            return Err(LoadError::EmptyFactory);
        }
        let get_desc = factory_ref
            .get_plugin_descriptor
            .ok_or(LoadError::MissingFunction {
                stage: Stage::Factory,
                name: "get_plugin_descriptor",
            })?;

        let mut descriptors = Vec::new();
        for index in 0..count {
            // SAFETY: the factory's own entry, with an index below its count.
            let desc = unsafe { get_desc(factory, index) };
            if desc.is_null() {
                continue;
            }
            // SAFETY: a non-null descriptor the factory returned, valid while
            // the image is mapped (`factory.h`).
            let d = unsafe { &*desc };
            // SAFETY: descriptor strings are null or nul-terminated and live
            // as long as the descriptor; `features` is a null-terminated array
            // of such strings, read at most `MAX_FEATURES` deep.
            let info = unsafe { describe(d) };
            descriptors.push(info);
        }
        if descriptors.is_empty() {
            return Err(LoadError::NoDescriptors { count });
        }
        Ok(Self {
            _entry: ManuallyDrop::new(entry_guard),
            library: ManuallyDrop::new(library),
            factory,
            descriptors,
        })
    }

    /// Every descriptor the factory advertised.
    #[must_use]
    pub fn descriptors(&self) -> &[DescriptorInfo] {
        &self.descriptors
    }

    /// The index of the descriptor to load: `plugin_id` if given, else the
    /// first, whose declared CLAP version must be readable.
    ///
    /// # Errors
    /// No descriptor has that id, or the chosen one's version is not
    /// readable.
    pub fn select(&self, plugin_id: Option<&str>) -> Result<usize, LoadError> {
        let index = select_index(&self.descriptors, plugin_id)?;
        check_version(
            self.descriptors[index].version_of_clap,
            "plugin descriptor",
            Stage::Factory,
        )?;
        Ok(index)
    }

    /// Creates and initialises descriptor `index`'s plugin on `host`.
    ///
    /// # Errors
    /// The factory cannot create it or its `init` fails; the half-made
    /// plugin is destroyed.
    ///
    /// # Panics
    /// If `index` is not a descriptor of this bundle.
    pub fn instantiate(
        self,
        index: usize,
        host: Host,
        roles: Arc<ThreadRoles>,
    ) -> Result<Plugin, LoadError> {
        let id = &self.descriptors[index].id;
        let id_cstr =
            CString::new(id.as_str()).map_err(|e| LoadError::InvalidPluginId(e.to_string()))?;
        // SAFETY: `factory` is the live factory read in `open`.
        let factory = unsafe { &*self.factory };
        let create = factory.create_plugin.ok_or(LoadError::MissingFunction {
            stage: Stage::Instantiation,
            name: "create_plugin",
        })?;
        // SAFETY: the factory's own entry, with the factory, a vtable that
        // outlives the plugin (`Plugin` owns `host` and destroys the plugin
        // first) and a nul-terminated id.
        let plugin = unsafe { create(self.factory, host.as_raw(), id_cstr.as_ptr()) };
        if plugin.is_null() {
            return Err(LoadError::CreateFailed);
        }
        Plugin::from_created(plugin, self, host, roles)
    }
}

/// Which descriptor `wanted` names: the one with that id, or the first when
/// none is named. An id nothing carries is an error, not a fallback to the
/// first, which would load a different plugin than the one asked for; a
/// descriptor without an id is never matched.
fn select_index(descriptors: &[DescriptorInfo], wanted: Option<&str>) -> Result<usize, LoadError> {
    let Some(wanted) = wanted else {
        return Ok(0);
    };
    descriptors
        .iter()
        .position(|d| !d.id.is_empty() && d.id == wanted)
        .ok_or_else(|| {
            let available = descriptors
                .iter()
                .map(|d| {
                    if d.id.is_empty() {
                        "<null id>".to_string()
                    } else {
                        d.id.clone()
                    }
                })
                .collect();
            LoadError::UnknownId {
                wanted: wanted.to_string(),
                available,
            }
        })
}

/// Reads a descriptor's strings.
///
/// # Safety
/// Its string fields are null or nul-terminated, and `features` is null or a
/// null-terminated array of such strings.
unsafe fn describe(d: &clap_plugin_descriptor) -> DescriptorInfo {
    const MAX_FEATURES: usize = 256;
    let mut features = Vec::new();
    if !d.features.is_null() {
        let mut at = d.features;
        // SAFETY: the array is null-terminated (the contract), so each read
        // stays before the terminator; the bound caps a missing terminator's
        // damage to `MAX_FEATURES` reads, which the contract forbids anyway.
        unsafe {
            while !(*at).is_null() && features.len() < MAX_FEATURES {
                features.push(lossy(*at));
                at = at.add(1);
            }
        }
    }
    // SAFETY: the contract.
    unsafe {
        DescriptorInfo {
            id: lossy(d.id),
            name: lossy(d.name),
            vendor: lossy(d.vendor),
            version: lossy(d.version),
            url: lossy(d.url),
            description: lossy(d.description),
            features,
            version_of_clap: d.clap_version,
        }
    }
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

    unsafe extern "C" fn reinit_counting_init(_path: *const c_char) -> bool {
        REINIT_CALLS.fetch_add(1, Ordering::SeqCst);
        true
    }

    unsafe extern "C" fn shared_counting_init(_path: *const c_char) -> bool {
        SHARED_CALLS.fetch_add(1, Ordering::SeqCst);
        true
    }

    unsafe extern "C" fn after_failure_counting_init(_path: *const c_char) -> bool {
        AFTER_FAILURE_CALLS.fetch_add(1, Ordering::SeqCst);
        true
    }

    unsafe extern "C" fn failing_init(_path: *const c_char) -> bool {
        false
    }

    /// `probe()` registers a path then drops the library, so the registry entry
    /// must NOT outlive the guard. A later `load()` of the same path re-dlopens a
    /// fresh image and MUST run `init` again; a `HashMap<PathBuf, bool>` would
    /// mark the path initialized forever and skip it, leaving the second image
    /// uninitialized.
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

#[cfg(test)]
mod select_tests {
    use super::*;

    /// A descriptor carrying just the id `select_index` matches on.
    ///
    /// The other fields stay empty: selection reads `id` and nothing else, and
    /// a fuller fixture would imply the function looks at more than it does.
    fn desc_with_id(id: &str) -> DescriptorInfo {
        DescriptorInfo {
            id: id.to_string(),
            name: String::new(),
            vendor: String::new(),
            version: String::new(),
            url: String::new(),
            description: String::new(),
            features: Vec::new(),
            version_of_clap: clap_sys::version::CLAP_VERSION,
        }
    }

    fn two() -> Vec<DescriptorInfo> {
        vec![
            desc_with_id("com.example.synth"),
            desc_with_id("com.example.fx"),
        ]
    }

    /// Naming no id loads the bundle's first plugin.
    ///
    /// This is what keeps every single-plugin bundle, which is nearly all of
    /// them, loading exactly as it did before ids were selectable.
    ///
    /// Mutation: return `descriptors.len() - 1` for `None`: fails.
    #[test]
    fn no_id_selects_the_first_descriptor() {
        assert_eq!(select_index(&two(), None).unwrap(), 0);
    }

    /// A named id selects that plugin, including one that is not first.
    ///
    /// The whole finding: `get_plugin_descriptor(factory, 0)` was hard-coded,
    /// so a bundle shipping a synth plus companion effects exposed only the
    /// synth, and the effects were unreachable with no error, since index 0
    /// loads fine.
    ///
    /// Mutation: ignore `wanted` and return 0: fails.
    #[test]
    fn a_named_id_selects_a_plugin_that_is_not_the_first() {
        let picked = select_index(&two(), Some("com.example.fx"))
            .expect("the second plugin in the bundle must be selectable");
        assert_eq!(two()[picked].id, "com.example.fx");
    }

    /// An id no descriptor carries is an error, not a silent fallback.
    ///
    /// Falling back to the first would load a *different plugin* than the one
    /// asked for: a session restoring "the compressor" would come back with the
    /// synth and no complaint. The message lists what the bundle does hold, so
    /// the caller can correct the id.
    ///
    /// Mutation: `unwrap_or(0)` in place of the error: fails.
    #[test]
    fn an_unknown_id_fails_rather_than_loading_something_else() {
        let err = select_index(&two(), Some("com.example.missing"))
            .expect_err("an unknown id must not silently load the first plugin");
        let msg = err.to_string();
        assert!(msg.contains("com.example.missing"), "names the wanted id");
        assert!(msg.contains("com.example.synth"), "lists what is available");
        assert!(msg.contains("com.example.fx"), "lists every available id");
    }

    /// A descriptor whose string fields are all null is read as empty
    /// strings, not dereferenced.
    ///
    /// `id` is a raw pointer straight from the plugin; a factory that leaves
    /// it null must not take the host down mid-selection.
    ///
    /// Mutation: have `lossy` call `CStr::from_ptr` without the null test:
    /// the null read crashes the test.
    #[test]
    fn a_null_id_is_skipped_rather_than_dereferenced() {
        // SAFETY: `clap_plugin_descriptor` is a version struct and pointers;
        // all-zero is valid (null strings, no features).
        let d: clap_plugin_descriptor = unsafe { std::mem::zeroed() };
        // SAFETY: every string field is null, which `describe` accepts.
        let info = unsafe { describe(&d) };
        assert!(info.id.is_empty() && info.name.is_empty() && info.features.is_empty());
        let good = desc_with_id("com.example.fx");
        assert_eq!(
            select_index(&[info, good], Some("com.example.fx")).unwrap(),
            1
        );
    }

    /// A descriptor with no id is skipped, not matched.
    ///
    /// `id` is a raw string straight from the plugin; a factory that leaves it
    /// null must not stop a valid sibling matching.
    ///
    /// Mutation: drop the `!d.id.is_empty()` test and select `""`: fails.
    #[test]
    fn a_null_id_is_skipped_rather_than_matched() {
        let descriptors = vec![desc_with_id(""), desc_with_id("com.example.fx")];
        assert_eq!(
            select_index(&descriptors, Some("com.example.fx")).unwrap(),
            1
        );
        assert!(select_index(&descriptors, Some("")).is_err());
    }

    /// `Bundle::open` and a `Bundle`'s drop each hold the registry lock across
    /// the image's mapping and unmapping and the claim that goes with it, so an
    /// opener can never see a claim outlive its image, nor a fresh image
    /// inherit one (`init` run twice on one image, or skipped on a new one).
    /// The test holds the lock and shows both wait for it.
    ///
    /// Mutation: take no lock in `Bundle`'s drop: the drop finishes while the
    /// test holds the lock and the first assertion fails; have `open` use a
    /// registry of its own, not the global one (`let mut local = None;`, passed
    /// to `acquire_in`): the opener never waits and the second fails.
    /// Not covered: that `open` takes the lock *before* it maps the image
    /// rather than at the acquire (an opener blocks at either, and the map
    /// call has no observable effect here); that order is the construction in
    /// `open`, argued at `acquire_in`.
    #[test]
    fn opening_and_dropping_a_bundle_take_the_registry_lock() {
        use std::sync::mpsc;
        use std::time::Duration;

        let path = tutti_fixtures::cdylib("tutti_clap_test_plugin", "tutti-clap-test-plugin");
        let bundle = Bundle::open(&path, None).expect("opens");
        let wait = Duration::from_millis(300);

        let held = registry();
        let (done, finished) = mpsc::channel();
        let dropper = std::thread::spawn(move || {
            drop(bundle);
            done.send(()).unwrap();
        });
        assert!(
            finished.recv_timeout(wait).is_err(),
            "the drop waits for the registry"
        );
        drop(held);
        finished
            .recv_timeout(Duration::from_secs(10))
            .expect("the drop ends");
        dropper.join().unwrap();

        let held = registry();
        let (done, finished) = mpsc::channel();
        let opener = std::thread::spawn(move || {
            let bundle = Bundle::open(&path, None).expect("opens");
            done.send(()).unwrap();
            drop(bundle);
        });
        assert!(
            finished.recv_timeout(wait).is_err(),
            "the open waits for the registry"
        );
        drop(held);
        finished
            .recv_timeout(Duration::from_secs(10))
            .expect("the open ends");
        opener.join().unwrap();
    }
}
