//! VST3 library loading and factory access.

use std::ffi::c_void;
use std::path::Path;
use std::sync::Arc;

use libloading::Library;
use vst3::Steinberg::{
    kResultOk, FIDString, FUnknown, IPluginFactory, IPluginFactory3, IPluginFactory3Trait,
    IPluginFactoryTrait, PClassInfo, PClassInfoW, PFactoryInfo, TUID,
};
use vst3::{ComPtr, ComWrapper, Interface};

use crate::com::HostApplication;
use crate::error::{LoadStage, Result, Vst3Error};
use crate::helpers::{c_str_to_string, guid_as_tuid, utf16_to_string};
use crate::host::module_entry::ModuleEntry;

type GetPluginFactoryFn = unsafe extern "system" fn() -> *mut IPluginFactory;

/// A loaded VST3 dynamic library with its `IPluginFactory` resolved.
///
/// Shared via [`Arc`] because a single bundle commonly exposes multiple plugin
/// classes, and each [`Vst3Loaded`](crate::Vst3Loaded) instance keeps its
/// originating library alive for the plugin's lifetime.
pub struct Vst3Library {
    // Declaration order IS teardown order. Rust drops fields top-to-bottom, and
    // the module contract is: release the factory (and everything it can call
    // back into), then run the module exit point, then unload the DSO. So the
    // factory pointers come first, the host context next (the factory may hold
    // a borrowed pointer to it), `_entry` after that, and `_library` last.
    factory: ComPtr<IPluginFactory>,
    /// `IPluginFactory3` when the plugin implements it — the version that adds
    /// `setHostContext` and `getClassInfoUnicode`. `None` for factories that
    /// stop at `IPluginFactory`/`IPluginFactory2`.
    factory3: Option<ComPtr<IPluginFactory3>>,
    /// The `IHostApplication` handed to `setHostContext`. The factory borrows it
    /// (it does not take ownership the way `IPluginBase::initialize` does), so
    /// this must outlive the factory.
    _host_context: ComWrapper<HostApplication>,
    /// The run loop lent to the plugin via `IRunLoop`. Owned here, at library
    /// scope, because the host context registers handlers with it before any
    /// editor exists and it must outlive every `HostPlugFrame`. Linux only —
    /// other platforms have an ambient OS run loop.
    #[cfg(target_os = "linux")]
    run_loop: std::sync::Arc<crate::com::run_loop::RunLoop>,
    /// Runs the paired module exit point (`bundleExit` / `ModuleExit` /
    /// `ExitDll`) on drop. Held only for that side effect.
    _entry: ModuleEntry,
    _library: Library,
}

/// Host name reported to plugins through `IHostApplication::getName`, both at
/// factory level (`setHostContext`) and per instance.
pub(crate) const HOST_NAME: &str = "vst3-host";

unsafe impl Send for Vst3Library {}
unsafe impl Sync for Vst3Library {}

impl Vst3Library {
    /// The run loop this library lends the plugin. Shared with every
    /// `HostPlugFrame` so handlers registered through either object are pumped
    /// by one loop. Linux only.
    #[cfg(target_os = "linux")]
    pub(crate) fn run_loop(&self) -> std::sync::Arc<crate::com::run_loop::RunLoop> {
        self.run_loop.clone()
    }

    /// Load a VST3 library from a pre-resolved path to the actual binary (the
    /// inner Mach-O / ELF / PE, not the bundle directory).
    ///
    /// # Errors
    ///
    /// Returns [`Vst3Error::LoadFailed`] if the OS cannot open the library, if
    /// the module's entry point refuses initialization, if the
    /// `GetPluginFactory` symbol is missing, or if the factory function returns
    /// null.
    pub fn load(lib_path: &Path) -> Result<Arc<Self>> {
        let library = unsafe {
            Library::new(lib_path).map_err(|e| Vst3Error::LoadFailed {
                path: lib_path.to_path_buf(),
                stage: LoadStage::Opening,
                reason: e.to_string(),
            })?
        };

        // The module entry point must run BEFORE `GetPluginFactory`: it is where
        // a plugin does its one-time module init and, on macOS, learns its own
        // CFBundleRef so it can find bundled resources (presets, wavetables,
        // licences). Skipping it produces plugins that load without error and
        // are quietly missing half their data. The paired exit runs on drop —
        // see the field-order note on `Vst3Library`.
        let entry =
            ModuleEntry::enter(&library, lib_path).map_err(|reason| Vst3Error::LoadFailed {
                path: lib_path.to_path_buf(),
                stage: LoadStage::Opening,
                reason,
            })?;

        let get_factory: libloading::Symbol<GetPluginFactoryFn> = unsafe {
            library
                .get(b"GetPluginFactory\0")
                .map_err(|e| Vst3Error::LoadFailed {
                    path: lib_path.to_path_buf(),
                    stage: LoadStage::Factory,
                    reason: format!("Missing GetPluginFactory symbol: {}", e),
                })?
        };

        let factory_ptr = unsafe { get_factory() };
        let factory =
            unsafe { ComPtr::from_raw(factory_ptr) }.ok_or_else(|| Vst3Error::LoadFailed {
                path: lib_path.to_path_buf(),
                stage: LoadStage::Factory,
                reason: "GetPluginFactory returned null".to_string(),
            })?;

        // `IPluginFactory3::setHostContext` — the only way a plugin can reach
        // `IHostApplication` *before* any instance exists. Plugins use it for
        // host-dependent licensing / feature gating during scanning and at
        // instantiation time (JUCE calls it on both its scan and load paths).
        // Optional by spec: a factory that only implements IPluginFactory or
        // IPluginFactory2 has nothing to set, which is not an error.
        let factory3 = factory.cast::<IPluginFactory3>();
        #[cfg(target_os = "linux")]
        let run_loop = crate::com::run_loop::RunLoop::new();
        let host_context = HostApplication::new(
            HOST_NAME,
            #[cfg(target_os = "linux")]
            run_loop.clone(),
        );
        if let Some(f3) = factory3.as_ref() {
            if let Some(app) = host_context.to_com_ptr::<vst3::Steinberg::Vst::IHostApplication>() {
                // The factory does NOT take ownership (unlike
                // `IPluginBase::initialize`), so hand it a borrowed pointer and
                // keep our own reference alive in `_host_context` for as long as
                // the factory can call back into it.
                let raw = app.upcast::<FUnknown>().as_ptr();
                unsafe { f3.setHostContext(raw) };
            }
        }

        Ok(Arc::new(Self {
            factory,
            factory3,
            _host_context: host_context,
            #[cfg(target_os = "linux")]
            run_loop,
            _entry: entry,
            _library: library,
        }))
    }

    /// Vendor/URL/email from the factory. `None` if the plugin rejects the
    /// `getFactoryInfo` call.
    pub fn get_factory_info(&self) -> Option<FactoryInfo> {
        let mut info: PFactoryInfo = unsafe { std::mem::zeroed() };
        let result = unsafe { self.factory.getFactoryInfo(&mut info) };
        if result == kResultOk {
            Some(FactoryInfo {
                vendor: c_str_to_string(&info.vendor),
                url: c_str_to_string(&info.url),
                email: c_str_to_string(&info.email),
            })
        } else {
            None
        }
    }

    /// Number of plugin classes exposed by this factory (audio processors,
    /// controllers, etc.). A single bundle may contain many.
    pub fn count_classes(&self) -> i32 {
        unsafe { self.factory.countClasses() }
    }

    /// Read the `index`-th class descriptor from the factory.
    ///
    /// Prefers `IPluginFactory3::getClassInfoUnicode`, whose `name` is UTF-16,
    /// falling back to the ASCII `IPluginFactory::getClassInfo`. The ASCII
    /// variant declares `char8` and gives no encoding, so any plugin with a
    /// non-ASCII display name ("Väst", "音源", "Éclat") comes back mangled
    /// through it — the unicode form is the only one that round-trips.
    ///
    /// # Errors
    ///
    /// Returns [`Vst3Error::PluginError`] if the factory rejects the index.
    /// A factory that implements `IPluginFactory3` but rejects the index in
    /// `getClassInfoUnicode` still gets the ASCII attempt before erroring.
    pub fn get_class_info(&self, index: i32) -> Result<ClassInfo> {
        if let Some(info) = self.class_info_unicode(index) {
            return Ok(info);
        }

        let mut info: PClassInfo = unsafe { std::mem::zeroed() };
        let result = unsafe { self.factory.getClassInfo(index, &mut info) };
        if result == kResultOk {
            let cid_bytes: [u8; 16] = unsafe { std::mem::transmute(info.cid) };
            Ok(ClassInfo {
                cid: info.cid,
                cid_bytes,
                category: c_str_to_string(&info.category),
                name: c_str_to_string(&info.name),
            })
        } else {
            Err(Vst3Error::PluginError {
                stage: LoadStage::Factory,
                code: result,
            })
        }
    }

    /// `IPluginFactory3::getClassInfoUnicode` for `index`, or `None` when the
    /// factory doesn't implement `IPluginFactory3` or rejects the index.
    ///
    /// `category` stays `char8` even in the unicode struct — it is a fixed set
    /// of ASCII spec constants ("Audio Module Class", "Component Controller
    /// Class"), so `c_str_to_string` is correct there.
    fn class_info_unicode(&self, index: i32) -> Option<ClassInfo> {
        let f3 = self.factory3.as_ref()?;
        let mut info: PClassInfoW = unsafe { std::mem::zeroed() };
        if unsafe { f3.getClassInfoUnicode(index, &mut info) } != kResultOk {
            return None;
        }
        let cid_bytes: [u8; 16] = unsafe { std::mem::transmute(info.cid) };
        Some(ClassInfo {
            cid: info.cid,
            cid_bytes,
            category: c_str_to_string(&info.category),
            name: utf16_to_string(&info.name),
        })
    }

    /// Instantiate a class by CID and query for an interface. Returns a
    /// `+1` refcounted raw pointer to the requested interface.
    pub(crate) fn create_instance<I: Interface>(&self, cid: &TUID) -> Result<ComPtr<I>> {
        let iid_tuid = guid_as_tuid(&I::IID);
        let mut obj: *mut c_void = std::ptr::null_mut();
        let result = unsafe {
            self.factory.createInstance(
                cid.as_ptr() as FIDString,
                iid_tuid.as_ptr() as FIDString,
                &mut obj,
            )
        };
        if result != kResultOk || obj.is_null() {
            return Err(Vst3Error::PluginError {
                stage: LoadStage::Instantiation,
                code: result,
            });
        }
        unsafe { ComPtr::from_raw(obj as *mut I) }.ok_or(Vst3Error::PluginError {
            stage: LoadStage::Instantiation,
            code: result,
        })
    }
}

/// Vendor identification read from `IPluginFactory::getFactoryInfo`.
#[derive(Debug, Clone)]
pub struct FactoryInfo {
    /// Vendor / company name.
    pub vendor: String,
    /// Vendor's website.
    pub url: String,
    /// Vendor contact email.
    pub email: String,
}

/// Descriptor for a single class (plugin variant) within a factory.
#[derive(Debug, Clone)]
pub struct ClassInfo {
    /// Steinberg-signed class id used to pass back to
    /// `IPluginFactory::createInstance`.
    pub cid: TUID,
    /// Human-readable byte-order independent representation for formatting.
    pub cid_bytes: [u8; 16],
    /// Category string, e.g. `"Audio Module Class"`, `"Controller Class"`.
    pub category: String,
    /// Display name of the class.
    pub name: String,
}
