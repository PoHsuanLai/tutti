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
            if let Some(app) = host_context.as_com_ref::<vst3::Steinberg::Vst::IHostApplication>() {
                // The factory does not take ownership — nor does
                // `IPluginBase::initialize`; both borrow, and retain for
                // themselves if they keep the context (see the contract note on
                // `Vst3Loaded::host_context_ptr`). So hand over a borrowed
                // pointer and keep our own reference alive in `_host_context`
                // for as long as the factory can call back into it.
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

    /// Vendor/URL/email and the factory flags. `None` if the plugin rejects the
    /// `getFactoryInfo` call.
    pub fn get_factory_info(&self) -> Option<FactoryInfo> {
        let mut info: PFactoryInfo = unsafe { std::mem::zeroed() };
        let result = unsafe { self.factory.getFactoryInfo(&mut info) };
        if result == kResultOk {
            Some(FactoryInfo {
                vendor: c_str_to_string(&info.vendor),
                url: c_str_to_string(&info.url),
                email: c_str_to_string(&info.email),
                flags: info.flags,
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
                // `PClassInfo` (the v1 struct) carries no vendor or version —
                // their absence is what `PClassInfo2`/`PClassInfoW` were added
                // for. `None` here is the honest answer, not a stub, and the
                // caller falls back to the factory's vendor.
                vendor: None,
                version: None,
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
            // `ipluginbase.h:357` documents the vendor field as "overwrite
            // vendor information from factory info", so a non-empty class
            // vendor outranks the factory's. Both are read here and the choice
            // is made at the call site, which is the only place that has both.
            //
            // Empty is the common case — most plugins fill only the factory —
            // and is why this is `Option` rather than a bare `String`: "the
            // class said nothing" and "the class said the empty string" would
            // otherwise be the same value, and only the first should fall back.
            vendor: non_empty(utf16_to_string(&info.vendor)),
            version: non_empty(utf16_to_string(&info.version)),
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

/// Vendor identification and factory flags, read from
/// `IPluginFactory::getFactoryInfo`.
#[derive(Debug, Clone)]
pub struct FactoryInfo {
    /// Vendor / company name.
    pub vendor: String,
    /// Vendor's website.
    pub url: String,
    /// Vendor contact email.
    pub email: String,
    /// Raw `PFactoryInfo::flags` bitmask — see [`factory_flags`] and
    /// [`classes_discardable`](Self::classes_discardable).
    ///
    /// Carried raw rather than decoded into bools. The four flags this host
    /// knows are not the four a future SDK defines, and `1 << 2` is already
    /// absent from the enum — a decode would silently drop whatever lands
    /// there, whereas a bitmask hands the caller exactly what the plugin said.
    pub flags: i32,
}

impl FactoryInfo {
    /// The plugin declares that its exported class list can change between
    /// loads (`kClassesDiscardable`), so a host must not answer "what does this
    /// bundle contain?" from a cache.
    ///
    /// **Nothing in tutti acts on this yet.** The scan cache
    /// (`tutti_plugin::host::discovery`) is keyed on file mtime alone, and it
    /// stores one descriptor per bundle path while `find_audio_class` takes
    /// only the first audio class — so a bundle's class *list* is not something
    /// the catalog can currently represent, let alone re-derive. Acting on the
    /// flag becomes meaningful when that changes; reading it is this crate's
    /// job either way, and dropping it here would leave the consumer with
    /// nothing to act on when it arrives.
    pub fn classes_discardable(&self) -> bool {
        self.flags & factory_flags::CLASSES_DISCARDABLE != 0
    }

    /// The plugin asks not to be unloaded before process exit
    /// (`kComponentNonDiscardable`).
    pub fn component_non_discardable(&self) -> bool {
        self.flags & factory_flags::COMPONENT_NON_DISCARDABLE != 0
    }

    /// The plugin's strings are Unicode (`kUnicode`) — true of every VST3
    /// plugin so far, per the SDK's own note on the flag.
    pub fn unicode_strings(&self) -> bool {
        self.flags & factory_flags::UNICODE != 0
    }
}

/// VST3 `PFactoryInfo::FactoryFlags` constants, mirroring `ipluginbase.h:65-83`.
///
/// `kLicenseCheck` (`1 << 1`) is deliberately absent: the SDK marks it
/// deprecated and says Cubase/Nuendo 12 and later ignore it, so a host that
/// reads it would be acting on a signal the format has withdrawn. `1 << 2` is
/// unassigned in the header.
/// `FactoryFlags` is `DefaultEnumType` — `u32` on unix, `c_int` on Windows —
/// while `PFactoryInfo::flags` is `int32` on both, so each cast below is a
/// no-op on one platform and load-bearing on the other. Same reason
/// [`crate::physical_ui_type`] carries this allow.
#[allow(clippy::unnecessary_cast)]
pub mod factory_flags {
    use vst3::Steinberg::PFactoryInfo_::FactoryFlags_;

    /// The exported class list can change each time the module is loaded, so
    /// class information must not be cached.
    pub const CLASSES_DISCARDABLE: i32 = FactoryFlags_::kClassesDiscardable as i32;
    /// The component will not be unloaded until process exit.
    pub const COMPONENT_NON_DISCARDABLE: i32 = FactoryFlags_::kComponentNonDiscardable as i32;
    /// The plugin's strings are entirely Unicode-encoded.
    pub const UNICODE: i32 = FactoryFlags_::kUnicode as i32;
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
    /// Per-class vendor, when the class declares one.
    ///
    /// `None` when the field is empty, which is the usual case — most plugins
    /// fill only the factory's vendor. The distinction matters: the header
    /// calls this an *overwrite* of the factory information, so an empty value
    /// must fall through to the factory rather than blank the vendor out.
    /// Distributor-published bundles are where the two differ.
    pub vendor: Option<String>,
    /// Per-class version string, e.g. `"1.0.0.512"`
    /// (Major.Minor.Subversion.Build). `None` when the class declares none.
    pub version: Option<String>,
}

/// `None` for an empty string, so "the plugin said nothing" is distinct from
/// "the plugin said the empty string" at every call site that must fall back.
fn non_empty(s: String) -> Option<String> {
    (!s.is_empty()).then_some(s)
}

#[cfg(test)]
mod factory_flag_tests {
    use super::{factory_flags, FactoryInfo};

    fn with_flags(flags: i32) -> FactoryInfo {
        FactoryInfo {
            vendor: String::new(),
            url: String::new(),
            email: String::new(),
            flags,
        }
    }

    /// Each accessor reads its own bit and no other.
    ///
    /// Every corpus plugin reports exactly `kUnicode`, so a decode that
    /// answered from the wrong bit — or from the whole field — would look
    /// correct against all 19 of them while misreporting the two flags none of
    /// them sets.
    #[test]
    fn each_factory_flag_accessor_reads_its_own_bit() {
        for (flags, want) in [
            (factory_flags::CLASSES_DISCARDABLE, (true, false, false)),
            (
                factory_flags::COMPONENT_NON_DISCARDABLE,
                (false, true, false),
            ),
            (factory_flags::UNICODE, (false, false, true)),
        ] {
            let info = with_flags(flags);
            let got = (
                info.classes_discardable(),
                info.component_non_discardable(),
                info.unicode_strings(),
            );
            assert_eq!(got, want, "flags=0x{flags:02x} decoded as {got:?}");
        }
    }

    /// A flag is read as one bit among several, not as the whole field.
    ///
    /// `kUnicode` is set by every plugin in the corpus, so an equality test
    /// against `kClassesDiscardable` would report `false` for a bundle that
    /// asked for both — which is the only combination that matters, since a
    /// discardable bundle is also a Unicode one.
    #[test]
    fn other_factory_flags_do_not_mask_classes_discardable() {
        let both = with_flags(factory_flags::CLASSES_DISCARDABLE | factory_flags::UNICODE);
        assert!(both.classes_discardable());
        assert!(both.unicode_strings());

        assert!(!with_flags(factory_flags::UNICODE).classes_discardable());
    }

    /// No flags set means no flag reads as set — pins that the accessors do not
    /// answer from a default.
    #[test]
    fn an_empty_flag_field_reports_nothing_set() {
        let none = with_flags(0);
        assert!(!none.classes_discardable());
        assert!(!none.component_non_discardable());
        assert!(!none.unicode_strings());
    }

    /// The bit values are the ones the SDK defines (`ipluginbase.h:65-83`).
    ///
    /// Pinned because everything above reads by mask: were a constant ever
    /// wrong, every assertion would still pass while silently matching a
    /// different flag. `1 << 1` (`kLicenseCheck`, deprecated) and `1 << 2`
    /// (unassigned) are deliberately absent from the module.
    #[test]
    fn the_factory_flag_bits_are_the_ones_the_sdk_defines() {
        assert_eq!(factory_flags::CLASSES_DISCARDABLE, 1 << 0);
        assert_eq!(factory_flags::COMPONENT_NON_DISCARDABLE, 1 << 3);
        assert_eq!(factory_flags::UNICODE, 1 << 4);
    }
}
