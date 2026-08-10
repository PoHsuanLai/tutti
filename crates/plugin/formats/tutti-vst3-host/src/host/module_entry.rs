//! The VST3 module entry / exit point — the per-DSO init hook that must run
//! before `GetPluginFactory` and the teardown hook that must run after the
//! factory is released.
//!
//! The VST3 SDK requires a host to call a platform-specific initializer once
//! per loaded module, paired with a matching finalizer:
//!
//! | Platform | entry | exit | signature |
//! |----------|-------|------|-----------|
//! | macOS    | `bundleEntry`  | `bundleExit`  | `bool (CFBundleRef)` |
//! | Linux/BSD| `ModuleEntry`  | `ModuleExit`  | `bool (void*)` |
//! | Windows  | `InitDll`      | `ExitDll`     | `bool ()` |
//!
//! (Cross-checked against JUCE's `DLLHandle` in
//! `juce_VST3PluginFormatImpl.h`, which resolves exactly these names.)
//!
//! Skipping the entry call is *silent*: the plugin still loads, still hands
//! back a factory, and still instantiates — it is simply missing the one
//! callback where it learns where it lives. On macOS `bundleEntry` is how a
//! plugin captures its own `CFBundleRef` to find bundled resources (factory
//! presets, wavetables, licence files, GUI assets), so the failure mode is
//! "plugin loads fine and then behaves as if half its data is missing".
//!
//! Both halves are best-effort by design: the SDK explicitly permits a module
//! to export no entry point at all (many simple plugins don't), so a missing
//! symbol is success, not an error. A symbol that *is* present and returns
//! `false` is a real refusal and aborts the load.

use std::path::Path;

/// The module-entry state for one loaded DSO. Holds whatever the exit call
/// needs and runs it on drop, so entry/exit stay paired even on an early
/// return or a panic between them.
pub(crate) struct ModuleEntry {
    inner: platform::Entry,
}

impl ModuleEntry {
    /// Run the platform's module entry point for the DSO `library` opened from
    /// `lib_path`.
    ///
    /// Returns `Err(reason)` only when the module exports an entry point and
    /// that entry point *refused* (returned false). A module with no entry
    /// point is a normal, supported case and yields `Ok`.
    pub(crate) fn enter(library: &libloading::Library, lib_path: &Path) -> Result<Self, String> {
        platform::enter(library, lib_path).map(|inner| Self { inner })
    }
}

impl Drop for ModuleEntry {
    fn drop(&mut self) {
        platform::exit(&mut self.inner);
    }
}

/// Resolve `name` as a function symbol in `library`, or `None` if the module
/// doesn't export it. A missing entry/exit point is a supported case.
fn symbol<'a, T>(
    library: &'a libloading::Library,
    name: &str,
) -> Option<libloading::Symbol<'a, T>> {
    unsafe { library.get(name).ok() }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{symbol, Path};
    use core_foundation_sys::base::{kCFAllocatorDefault, CFRelease};
    use core_foundation_sys::bundle::{CFBundleCreate, CFBundleRef};
    use core_foundation_sys::url::{CFURLCreateFromFileSystemRepresentation, CFURLRef};
    use std::os::unix::ffi::OsStrExt;

    type BundleEntryFn = unsafe extern "C" fn(CFBundleRef) -> bool;
    type BundleExitFn = unsafe extern "C" fn() -> bool;

    /// macOS keeps the `CFBundleRef` alive until `bundleExit` has run: the
    /// plugin is entitled to hold the reference it was handed for the module's
    /// whole lifetime.
    pub(super) struct Entry {
        bundle: CFBundleRef,
        /// Resolved `bundleExit`, if the module exports one. Stored as a raw fn
        /// pointer rather than a `Symbol` so `Entry` doesn't borrow the library.
        exit: Option<BundleExitFn>,
    }

    /// Walk up from the inner Mach-O (`Foo.vst3/Contents/MacOS/Foo`) to the
    /// `.vst3` bundle directory it lives in. Returns `None` for a bare dylib
    /// with no surrounding bundle.
    pub(super) fn bundle_dir(lib_path: &Path) -> Option<&Path> {
        lib_path
            .ancestors()
            .find(|p| p.extension().is_some_and(|e| e == "vst3") && p.is_dir())
    }

    /// Build a `CFBundleRef` for a `.vst3` bundle directory, or `None` if
    /// CoreFoundation rejects the path or cannot open it as a bundle.
    ///
    /// Ownership follows the CoreFoundation Create Rule: the returned reference
    /// is `+1` and the caller must `CFRelease` it exactly once. The intermediate
    /// `CFURLRef` is released here.
    ///
    /// # Safety
    ///
    /// Calls into the CoreFoundation C API, so it must run on a process where
    /// that framework is loaded (macOS). The caller owns the returned reference
    /// and must not release it more than once, nor use it after releasing.
    unsafe fn make_bundle(dir: &Path) -> Option<CFBundleRef> {
        let bytes = dir.as_os_str().as_bytes();
        let url: CFURLRef = CFURLCreateFromFileSystemRepresentation(
            kCFAllocatorDefault,
            bytes.as_ptr(),
            bytes.len() as isize,
            true as u8, // isDirectory — a .vst3 bundle always is
        );
        if url.is_null() {
            return None;
        }
        let bundle = CFBundleCreate(kCFAllocatorDefault, url);
        CFRelease(url as *const _);
        (!bundle.is_null()).then_some(bundle)
    }

    pub(super) fn enter(library: &libloading::Library, lib_path: &Path) -> Result<Entry, String> {
        // `bundleEntry` takes the plugin's own CFBundleRef; without a real
        // bundle there is nothing honest to pass, and handing a plugin a null
        // CFBundleRef invites it to dereference it. Skip the call instead.
        let Some(dir) = bundle_dir(lib_path) else {
            return Ok(Entry {
                bundle: std::ptr::null_mut(),
                exit: None,
            });
        };
        let Some(bundle) = (unsafe { make_bundle(dir) }) else {
            return Ok(Entry {
                bundle: std::ptr::null_mut(),
                exit: None,
            });
        };

        // Resolve the exit half BEFORE calling entry, so a module that
        // initialises successfully is never left without its finalizer.
        let exit = symbol::<BundleExitFn>(library, "bundleExit").map(|s| *s);

        if let Some(entry) = symbol::<BundleEntryFn>(library, "bundleEntry") {
            if !unsafe { entry(bundle) } {
                unsafe { CFRelease(bundle as *const _) };
                return Err("bundleEntry returned false".to_string());
            }
        }
        // No `bundleEntry` export: supported, nothing to do. The bundle ref is
        // still released on drop.

        Ok(Entry { bundle, exit })
    }

    pub(super) fn exit(entry: &mut Entry) {
        if let Some(exit) = entry.exit.take() {
            unsafe { exit() };
        }
        if !entry.bundle.is_null() {
            unsafe { CFRelease(entry.bundle as *const _) };
            entry.bundle = std::ptr::null_mut();
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod platform {
    use super::{symbol, Path};
    use std::ffi::c_void;

    type ModuleEntryFn = unsafe extern "C" fn(*mut c_void) -> bool;
    type ModuleExitFn = unsafe extern "C" fn() -> bool;

    pub(super) struct Entry {
        exit: Option<ModuleExitFn>,
        /// A second, independently-owned `dlopen` reference on the same DSO,
        /// held for as long as the plugin may still use the handle it was passed.
        /// `None` when the module exports no `ModuleEntry`. See [`enter`].
        handle: Option<libloading::os::unix::Library>,
    }

    pub(super) fn enter(library: &libloading::Library, lib_path: &Path) -> Result<Entry, String> {
        // Resolve the exit half BEFORE calling entry, so a module that
        // initialises successfully is never left without its finalizer.
        let exit = symbol::<ModuleExitFn>(library, "ModuleExit").map(|s| *s);

        let Some(entry) = symbol::<ModuleEntryFn>(library, "ModuleEntry") else {
            // No `ModuleEntry` export. The SDK's own loader treats this as fatal
            // on Linux, but tutti is a library and a module that never asks to
            // be initialised is harmless here — matching the macOS and Windows
            // arms, where a missing entry point is likewise a supported no-op.
            return Ok(Entry { exit, handle: None });
        };

        // `ModuleEntry` receives the module's own `dlopen` handle — the SDK's
        // `module_linux.cpp` passes its `mModule` verbatim — so the plugin can
        // locate itself via `dladdr` and find its bundled resources.
        //
        // `libloading::Library` will not lend that pointer out: the only
        // accessors are the consuming `into_raw`/`close`, and consuming the
        // caller's library here would either leak it or unload it out from under
        // the factory. So take a second reference by opening the same path
        // again. `dlopen` refcounts per path, so this hands back the *same*
        // handle rather than mapping a second copy — which is precisely the
        // value the contract wants — and the extra reference is released when
        // this `Entry` drops, after `ModuleExit` has run.
        //
        // A symbol address would not substitute: `dladdr` can map one back to
        // the module, but it is not the handle value the SDK specifies.
        let handle_raw = unsafe { libloading::os::unix::Library::new(lib_path) }
            .map_err(|e| format!("reopening the module for ModuleEntry failed: {e}"))?
            .into_raw();

        // Reconstruct the owner immediately, so the reference is released by
        // `Entry`'s drop on every path below — including the refusal below,
        // where `?`-style early return would otherwise leak it.
        let handle = unsafe { libloading::os::unix::Library::from_raw(handle_raw) };

        if !unsafe { entry(handle_raw) } {
            return Err("ModuleEntry returned false".to_string());
        }

        Ok(Entry {
            exit,
            handle: Some(handle),
        })
    }

    pub(super) fn exit(entry: &mut Entry) {
        if let Some(exit) = entry.exit.take() {
            unsafe { exit() };
        }
        // Release the extra `dlopen` reference only after `ModuleExit` has run:
        // the plugin is entitled to use the handle it was given right up to that
        // call, and on the last reference `dlclose` unmaps the image.
        drop(entry.handle.take());
    }
}

#[cfg(windows)]
mod platform {
    use super::{symbol, Path};

    type InitDllFn = unsafe extern "system" fn() -> bool;
    type ExitDllFn = unsafe extern "system" fn() -> bool;

    pub(super) struct Entry {
        exit: Option<ExitDllFn>,
    }

    pub(super) fn enter(library: &libloading::Library, _lib_path: &Path) -> Result<Entry, String> {
        let exit = symbol::<ExitDllFn>(library, "ExitDll").map(|s| *s);

        if let Some(entry) = symbol::<InitDllFn>(library, "InitDll") {
            if !unsafe { entry() } {
                return Err("InitDll returned false".to_string());
            }
        }

        Ok(Entry { exit })
    }

    pub(super) fn exit(entry: &mut Entry) {
        if let Some(exit) = entry.exit.take() {
            unsafe { exit() };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Opens the current process image as a stand-in "module that exports no
    /// VST3 entry points".
    fn this_process() -> libloading::Library {
        #[cfg(unix)]
        {
            libloading::os::unix::Library::this().into()
        }
        #[cfg(windows)]
        {
            libloading::os::windows::Library::this()
                .expect("current module handle")
                .into()
        }
    }

    /// A module with no entry-point export is the common case (the SDK
    /// explicitly permits it, and JUCE treats a missing symbol as success), so
    /// it must load cleanly rather than error.
    #[test]
    fn missing_entry_point_is_not_an_error() {
        let lib = this_process();
        let entry = ModuleEntry::enter(
            &lib,
            Path::new("/nonexistent/Fake.vst3/Contents/MacOS/Fake"),
        );
        assert!(
            entry.is_ok(),
            "a module without an entry point must load, not fail"
        );
        // Dropping runs the (absent) exit half; must not panic or double-free.
        drop(entry);
    }

    /// Entering twice and dropping both must be safe — a single bundle commonly
    /// backs several `Vst3Library` loads.
    #[test]
    fn entry_and_exit_are_paired_per_load() {
        let lib = this_process();
        let a = ModuleEntry::enter(&lib, Path::new("/nonexistent/Fake.vst3")).unwrap();
        let b = ModuleEntry::enter(&lib, Path::new("/nonexistent/Fake.vst3")).unwrap();
        drop(a);
        drop(b);
    }

    /// The macOS entry needs the plugin's own `.vst3` bundle directory, walked
    /// up from the inner Mach-O. Only a directory that actually exists counts.
    #[cfg(target_os = "macos")]
    #[test]
    fn bundle_dir_is_found_from_the_inner_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("Reverb.vst3");
        let macos = bundle.join("Contents/MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        let inner = macos.join("Reverb");
        std::fs::write(&inner, b"").unwrap();

        assert_eq!(super::platform::bundle_dir(&inner), Some(bundle.as_path()));

        // A bare dylib with no surrounding bundle yields None, which makes
        // `enter` skip `bundleEntry` rather than hand the plugin a null
        // CFBundleRef to dereference.
        let bare = tmp.path().join("libplain.dylib");
        std::fs::write(&bare, b"").unwrap();
        assert_eq!(super::platform::bundle_dir(&bare), None);

        // A `.vst3` component that is a *file*, not a directory, is not a
        // bundle either.
        let notabundle = tmp.path().join("Fake.vst3");
        std::fs::write(&notabundle, b"").unwrap();
        assert_eq!(super::platform::bundle_dir(&notabundle), None);
    }
}
