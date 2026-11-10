//! The loaded module and its factory: `dlopen`, the module entry point,
//! `GetPluginFactory`, the class list and `createInstance`.

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use libloading::Library;
use vst3::Steinberg::{
    FIDString, FUnknown, IBStream, IPluginCompatibility, IPluginCompatibilityTrait, IPluginFactory,
    IPluginFactory3, IPluginFactory3Trait, IPluginFactoryTrait, PClassInfo, PClassInfoW,
    PFactoryInfo, TUID, Vst::IEditController, Vst::IHostApplication,
};
use vst3::{ComPtr, ComWrapper, Interface};

use tutti_plugin_types::LoadStage;

use crate::com::{BStream, HostApplication};
use crate::error::{Vst3Error, check};
use crate::module_entry::ModuleEntry;
use crate::plugin::{Component, Plugin};
use crate::sdk::{c_str_to_string, guid_as_tuid, utf16_to_string, zeroed};

type GetPluginFactoryFn = unsafe extern "system" fn() -> *mut IPluginFactory;

/// Host name reported to plugins through `IHostApplication::getName`, both at
/// factory level (`setHostContext`) and per instance.
pub const HOST_NAME: &str = "vst3-host";

/// Why a module could not be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadError {
    /// Which loading step failed.
    pub stage: LoadStage,
    /// Human-readable cause, for logs rather than matching.
    pub reason: String,
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.stage, self.reason)
    }
}

impl std::error::Error for LoadError {}

/// Why a plugin could not be created from a class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CreateError {
    /// `createInstance` refused or returned nothing.
    Create(Vst3Error),
    /// The component does not implement `IAudioProcessor`, which a host
    /// cannot render without.
    NoAudioProcessor,
}

/// A loaded VST3 dynamic library with its `IPluginFactory` resolved.
///
/// Shared via [`Arc`] because a single bundle commonly exposes multiple plugin
/// classes, and each [`Plugin`] keeps its originating module alive for the
/// plugin's lifetime.
pub struct Module {
    /// Dropped by hand in `Drop`, under [`MODULE_LOCK`].
    parts: ManuallyDrop<Parts>,
}

/// What a [`Module`] owns.
struct Parts {
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
    /// The run loop lent to the plugin via `IRunLoop`. Owned here, at module
    /// scope, because the host context registers handlers with it before any
    /// editor exists and it must outlive every `HostPlugFrame`. Linux only —
    /// other platforms have an ambient OS run loop.
    #[cfg(target_os = "linux")]
    run_loop: Arc<crate::com::RunLoop>,
    /// Runs the paired module exit point (`bundleExit` / `ModuleExit` /
    /// `ExitDll`) on drop. Held only for that side effect.
    _entry: ModuleEntry,
    _library: Library,
}

impl std::fmt::Debug for Module {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Module")
            .field("_library", &self.parts._library)
            .finish_non_exhaustive()
    }
}

// SAFETY: `ComPtr` is a raw pointer, so neither trait is inferred. One module
// backs several `Plugin`s, and `countClasses`, `getClassInfo` and
// `createInstance` may be called concurrently (the SDK's own hosts scan from
// worker threads). Towards the host the factory is read-only once built:
// `setHostContext`, its one mutating call, runs inside `open` under `MODULE_LOCK`,
// before the `Arc` exists. The other fields never move
// under a shared reference: `_library` unloads only in `Drop`, which `Arc`
// serialises (and `MODULE_LOCK` orders against every other open and drop), and `run_loop` is `Mutex`-guarded (see `com/run_loop.rs`).
unsafe impl Send for Module {}
// SAFETY: see `Send` above.
unsafe impl Sync for Module {}

/// Held across the whole of [`Module::open`] and the whole of a [`Module`]'s
/// teardown (factory release, module exit, unload).
///
/// The SDK's plugin side keeps process-global state that both mutate with no
/// lock of its own: `LinuxPlatformTimer::runLoop`, `gPluginFactory` and
/// `moduleCounter`, shared by every open of one image. `GetPluginFactory` adds
/// a reference to `gPluginFactory` while it is set, and `~CPluginFactory`
/// clears it, so an open can race a dying factory; `moduleCounter` is a plain
/// `int`. Locking `setHostContext` alone still raced, and so did an open
/// without the drop.
///
/// A plain mutex, so the holder must never drop a `Module`: `open` builds one
/// only as its last step, after which it returns. Cost: a plugin that hangs in
/// its module entry, `GetPluginFactory` or module exit blocks every other VST3
/// open and close in the process.
static MODULE_LOCK: Mutex<()> = Mutex::new(());

fn lock_modules() -> std::sync::MutexGuard<'static, ()> {
    MODULE_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Drop for Module {
    fn drop(&mut self) {
        let _one_at_a_time = lock_modules();
        // Whatever is still registered with the run loop (a registration
        // through the module's own context belongs to no instance) points into
        // code about to be unloaded.
        #[cfg(target_os = "linux")]
        self.parts.run_loop.clear_all();
        // SAFETY: `parts` is dropped here and nowhere else, and never used
        // again. Dropping it by hand is what lets the guard outlive the fields.
        unsafe { ManuallyDrop::drop(&mut self.parts) };
    }
}

impl Module {
    /// The run loop this module lends the plugin. Shared with every
    /// `HostPlugFrame` so handlers registered through either object are pumped
    /// by one loop. Linux only.
    #[cfg(target_os = "linux")]
    pub(crate) fn run_loop(&self) -> Arc<crate::com::RunLoop> {
        self.parts.run_loop.clone()
    }

    /// The host application a plugin created from this module is handed, with
    /// the module's run loop. One per instance.
    pub(crate) fn new_host_application(
        &self,
        #[cfg(target_os = "linux")] owner: crate::com::Owner,
    ) -> ComWrapper<HostApplication> {
        HostApplication::new(
            HOST_NAME,
            #[cfg(target_os = "linux")]
            self.parts.run_loop.clone(),
            #[cfg(target_os = "linux")]
            Some(owner),
        )
    }

    /// Opens a VST3 module from a pre-resolved path to the actual binary (the
    /// inner Mach-O / ELF / PE, not the bundle directory).
    ///
    /// # Errors
    ///
    /// [`LoadError`] if the OS cannot open the library, if the module's entry
    /// point refuses initialization, if the `GetPluginFactory` symbol is
    /// missing, or if the factory function returns null.
    ///
    /// Opens and teardowns are serialised process-wide; see `MODULE_LOCK`.
    pub fn open(lib_path: &Path) -> Result<Arc<Self>, LoadError> {
        let _one_at_a_time = lock_modules();
        // SAFETY: loading a plugin runs its static initializers, which is what
        // the caller asked for by naming a plugin module; `Library::new`'s
        // contract is that the path is a library the process may run.
        let library = unsafe { Library::new(lib_path) }.map_err(|e| LoadError {
            stage: LoadStage::Opening,
            reason: e.to_string(),
        })?;

        // The module entry point must run BEFORE `GetPluginFactory`: it is where
        // a plugin does its one-time module init and, on macOS, learns its own
        // CFBundleRef so it can find bundled resources (presets, wavetables,
        // licences). Skipping it produces plugins that load without error and
        // are quietly missing half their data. The paired exit runs on drop —
        // see the field-order note on `Module`.
        let entry = ModuleEntry::enter(&library, lib_path)?;

        // SAFETY: the symbol is the VST3 module's `GetPluginFactory`, whose C
        // signature is `GetPluginFactoryFn`; the symbol borrows `library`, which
        // outlives this block.
        let get_factory: libloading::Symbol<GetPluginFactoryFn> =
            unsafe { library.get(b"GetPluginFactory\0") }.map_err(|e| LoadError {
                stage: LoadStage::Factory,
                reason: format!("Missing GetPluginFactory symbol: {}", e),
            })?;

        // SAFETY: `GetPluginFactory` takes no arguments and returns a +1
        // reference to the module's factory, or null.
        let factory_ptr = unsafe { get_factory() };
        // SAFETY: a non-null result is that +1 reference, which `ComPtr` adopts
        // and releases on drop.
        let factory = unsafe { ComPtr::from_raw(factory_ptr) }.ok_or_else(|| LoadError {
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
        let run_loop = crate::com::RunLoop::new();
        let host_context = HostApplication::new(
            HOST_NAME,
            #[cfg(target_os = "linux")]
            run_loop.clone(),
            // The module's own context serves every plugin: it has no owner.
            #[cfg(target_os = "linux")]
            None,
        );
        if let Some(f3) = factory3.as_ref()
            && let Some(app) = host_context.as_com_ref::<IHostApplication>()
        {
            // The factory does not take ownership — nor does
            // `IPluginBase::initialize`; both borrow, and retain for
            // themselves if they keep the context. So hand over a borrowed
            // pointer and keep this host's own reference alive in `_host_context`
            // for as long as the factory can call back into it.
            let raw = app.upcast::<FUnknown>().as_ptr();
            // SAFETY: `raw` is the live host application, which `_host_context`
            // keeps alive for as long as the factory does (field order).
            unsafe { f3.setHostContext(raw) };
        }

        Ok(Arc::new(Self {
            parts: ManuallyDrop::new(Parts {
                factory,
                factory3,
                _host_context: host_context,
                #[cfg(target_os = "linux")]
                run_loop,
                _entry: entry,
                _library: library,
            }),
        }))
    }

    /// Vendor/URL/email and the factory flags. `None` if the plugin rejects the
    /// `getFactoryInfo` call.
    #[must_use]
    pub fn factory_info(&self) -> Option<FactoryInfo> {
        let mut info: PFactoryInfo = zeroed();
        // SAFETY: `info` is a valid out-parameter for the call.
        let result = unsafe { self.parts.factory.getFactoryInfo(&mut info) };
        check(result).ok().map(|()| FactoryInfo::from_c(&info))
    }

    /// Number of plugin classes exposed by this factory (audio processors,
    /// controllers, etc.). A single bundle may contain many.
    #[must_use]
    pub fn class_count(&self) -> i32 {
        // SAFETY: a plain getter on a live factory.
        unsafe { self.parts.factory.countClasses() }
    }

    /// Every class the factory describes, in index order.
    ///
    /// A class whose descriptor the factory refuses is skipped; use
    /// [`class_info`](Self::class_info) to see why.
    pub fn classes(&self) -> impl Iterator<Item = ClassInfo> + '_ {
        (0..self.class_count()).filter_map(|i| self.class_info(i).ok())
    }

    /// Reads the `index`-th class descriptor from the factory.
    ///
    /// Prefers `IPluginFactory3::getClassInfoUnicode`, whose `name` is UTF-16,
    /// falling back to the ASCII `IPluginFactory::getClassInfo`. The ASCII
    /// variant declares `char8` and gives no encoding, so any plugin with a
    /// non-ASCII display name ("Väst", "音源", "Éclat") comes back mangled
    /// through it — the unicode form is the only one that round-trips.
    ///
    /// # Errors
    ///
    /// The factory's error if it rejects the index. A factory that implements
    /// `IPluginFactory3` but rejects the index in `getClassInfoUnicode` still
    /// gets the ASCII attempt before erroring.
    pub fn class_info(&self, index: i32) -> Result<ClassInfo, Vst3Error> {
        if let Some(info) = self.class_info_unicode(index) {
            return Ok(info);
        }

        let mut info: PClassInfo = zeroed();
        // SAFETY: `info` is a valid out-parameter for the call.
        check(unsafe { self.parts.factory.getClassInfo(index, &mut info) })?;
        Ok(ClassInfo {
            cid: info.cid,
            cid_bytes: info.cid.map(|b| b as u8),
            category: c_str_to_string(&info.category),
            name: c_str_to_string(&info.name),
            // `PClassInfo` (the v1 struct) carries no vendor, version or
            // subcategories — their absence is what `PClassInfo2`/
            // `PClassInfoW` were added for. `None` here is the honest
            // answer, not a stub, and the caller falls back to the
            // factory's vendor.
            vendor: None,
            version: None,
            sub_categories: None,
        })
    }

    /// `IPluginFactory3::getClassInfoUnicode` for `index`, or `None` when the
    /// factory doesn't implement `IPluginFactory3` or rejects the index.
    ///
    /// `category` stays `char8` even in the unicode struct — it is a fixed set
    /// of ASCII spec constants ("Audio Module Class", "Component Controller
    /// Class"), so `c_str_to_string` is correct there.
    fn class_info_unicode(&self, index: i32) -> Option<ClassInfo> {
        let f3 = self.parts.factory3.as_ref()?;
        let mut info: PClassInfoW = zeroed();
        // SAFETY: `info` is a valid out-parameter for the call.
        check(unsafe { f3.getClassInfoUnicode(index, &mut info) }).ok()?;
        Some(ClassInfo {
            cid: info.cid,
            cid_bytes: info.cid.map(|b| b as u8),
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
            // `char8` even in the unicode struct, like `category` above: the
            // subcategory vocabulary is a set of ASCII spec constants
            // (`ivstprocesscontext.h`'s `PlugType`), so `c_str_to_string` is
            // correct rather than a UTF-16 decode.
            sub_categories: non_empty(c_str_to_string(&info.subCategories)),
        })
    }

    /// Instantiates a class by CID and queries it for an interface.
    fn create_instance<I: Interface>(&self, cid: &TUID) -> Result<ComPtr<I>, Vst3Error> {
        let iid_tuid = guid_as_tuid(&I::IID);
        let mut obj: *mut c_void = std::ptr::null_mut();
        // SAFETY: both ids are 16-byte arrays alive for the call, and `obj` is
        // a valid out-parameter.
        let result = unsafe {
            self.parts.factory.createInstance(
                cid.as_ptr() as FIDString,
                iid_tuid.as_ptr() as FIDString,
                &mut obj,
            )
        };
        check(result)?;
        // SAFETY: a successful `createInstance` returns a +1 reference to the
        // requested interface `I`, or null, which `from_raw` refuses.
        unsafe { ComPtr::from_raw(obj as *mut I) }.ok_or(Vst3Error::InternalError)
    }

    /// Creates the component of a class and releases it again, to find out
    /// whether the class instantiates at all. Calls nothing on it.
    ///
    /// # Errors
    ///
    /// The factory's error.
    pub fn check_instantiable(&self, cid: &[i8; 16]) -> Result<(), Vst3Error> {
        self.create_component(cid).map(drop)
    }

    /// Creates the component of a class.
    ///
    /// Crate-private because the component is a bare COM reference: it does not
    /// keep the module loaded, so it must not outlive the `Arc<Module>` it came
    /// from. Callers here either drop it at once or hand it to a [`Plugin`],
    /// which holds the module.
    pub(crate) fn create_component(&self, cid: &[i8; 16]) -> Result<Component, Vst3Error> {
        self.create_instance::<vst3::Steinberg::Vst::IComponent>(cid)
            .map(Component::new)
    }

    /// Creates a plugin from an audio class: the component, its audio
    /// processor and its edit controller, not yet initialized.
    ///
    /// # Errors
    ///
    /// [`CreateError`] if the factory refuses the class or the component has no
    /// `IAudioProcessor`.
    pub fn create_plugin(self: &Arc<Self>, cid: &[i8; 16]) -> Result<Plugin, CreateError> {
        let component = self.create_component(cid).map_err(CreateError::Create)?;
        Plugin::assemble(self, component)
    }

    /// Creates the edit controller of a component whose controller is a
    /// separate class.
    pub(crate) fn create_controller(
        &self,
        cid: &[i8; 16],
    ) -> Result<ComPtr<IEditController>, Vst3Error> {
        self.create_instance::<IEditController>(cid)
    }

    /// The compatibility JSON of the module's `Plugin Compatibility Class`, if
    /// it exports one and the class answers.
    #[must_use]
    pub fn compatibility_json(&self) -> Option<String> {
        const COMPATIBILITY_CATEGORY: &str = "Plugin Compatibility Class";

        let cid = self
            .classes()
            .find_map(|info| (info.category == COMPATIBILITY_CATEGORY).then_some(info.cid))?;
        let compat = self.create_instance::<IPluginCompatibility>(&cid).ok()?;

        let stream = BStream::new();
        let stream_ptr = stream.as_com_ref::<IBStream>()?;
        // SAFETY: the stream is a live host object that `stream` keeps alive
        // across the call.
        check(unsafe { compat.getCompatibilityJSON(stream_ptr.as_ptr()) }).ok()?;
        let bytes = stream.data();
        (!bytes.is_empty()).then(|| String::from_utf8_lossy(&bytes).into_owned())
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
    /// The plugin's `PFactoryInfo::flags`, including bits this host does not
    /// name (a decode would drop whatever a future SDK adds), see
    /// [`classes_discardable`](Self::classes_discardable).
    pub flags: FactoryFlags,
}

impl FactoryInfo {
    /// Reads the plugin's `PFactoryInfo`, keeping every flag bit it sent.
    fn from_c(info: &PFactoryInfo) -> Self {
        Self {
            vendor: c_str_to_string(&info.vendor),
            url: c_str_to_string(&info.url),
            email: c_str_to_string(&info.email),
            flags: FactoryFlags::from_bits_retain(info.flags),
        }
    }

    /// The plugin declares that its exported class list can change between
    /// loads (`kClassesDiscardable`), so a host must not answer "what does this
    /// bundle contain?" from a cache. Nothing in tutti acts on it yet: the scan
    /// cache keys on mtime and holds one descriptor per bundle.
    pub fn classes_discardable(&self) -> bool {
        self.flags.contains(FactoryFlags::CLASSES_DISCARDABLE)
    }

    /// The plugin asks not to be unloaded before process exit
    /// (`kComponentNonDiscardable`).
    pub fn component_non_discardable(&self) -> bool {
        self.flags.contains(FactoryFlags::COMPONENT_NON_DISCARDABLE)
    }

    /// The plugin's strings are Unicode (`kUnicode`) — true of every VST3
    /// plugin so far, per the SDK's own note on the flag.
    pub fn unicode_strings(&self) -> bool {
        self.flags.contains(FactoryFlags::UNICODE)
    }
}

use vst3::Steinberg::PFactoryInfo_::FactoryFlags_;

bitflags::bitflags! {
    /// `PFactoryInfo::flags`, mirroring `ipluginbase.h:65-83`.
    ///
    /// `kLicenseCheck` (`1 << 1`) is deliberately absent: the SDK marks it
    /// deprecated and says Cubase/Nuendo 12 and later ignore it, so a host that
    /// reads it would be acting on a signal the format has withdrawn. `1 << 2`
    /// is unassigned in the header. A set read from a plugin keeps whatever bits
    /// it sent.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct FactoryFlags: i32 {
        /// The exported class list can change each time the module is loaded,
        /// so class information must not be cached.
        const CLASSES_DISCARDABLE = crate::sdk_enum_i32(FactoryFlags_::kClassesDiscardable);
        /// The component will not be unloaded until process exit.
        const COMPONENT_NON_DISCARDABLE =
            crate::sdk_enum_i32(FactoryFlags_::kComponentNonDiscardable);
        /// The plugin's strings are entirely Unicode-encoded.
        const UNICODE = crate::sdk_enum_i32(FactoryFlags_::kUnicode);
        // The plugin's own bits beyond the ones named.
        const _ = !0;
    }
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
    ///
    /// This names what *kind of COM class* this is, not what the plugin does —
    /// every audio plugin reports `"Audio Module Class"`. The musical taxonomy
    /// ("Fx|Reverb", "Instrument|Synth") is [`sub_categories`](Self::sub_categories).
    pub category: String,
    /// Display name of the class.
    pub name: String,
    /// The plugin's declared subcategories, `|`-delimited, e.g. `"Fx|Reverb"`
    /// or `"Instrument|Synth"` (`ipluginbase.h:355`).
    ///
    /// `None` when the factory implements only `IPluginFactory` (the v1 struct
    /// carries no such field) or when the class declares none. That is the same
    /// distinction [`vendor`](Self::vendor) draws, and for the same reason: a
    /// consumer classifying a plugin must be able to tell "the factory is too
    /// old to say" from "the plugin says it has no subcategory", and an empty
    /// string spells both.
    pub sub_categories: Option<String>,
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
    use super::{FactoryFlags, FactoryInfo};
    use crate::sdk::zeroed;

    /// **A bit the plugin sets that this host does not name survives the
    /// decode**, so a caller sees exactly what the plugin said.
    ///
    /// Mutation (run, fails): `from_bits_truncate` in `FactoryInfo::from_c`
    /// together with removing `FactoryFlags`'s `const _ = !0` -> the unnamed bit
    /// is gone (`from_bits_truncate` alone passes: the const names every bit).
    #[test]
    fn an_unnamed_factory_flag_bit_survives() {
        let mut raw: vst3::Steinberg::PFactoryInfo = zeroed();
        raw.flags = FactoryFlags::UNICODE.bits() | (1 << 9);
        let info = FactoryInfo::from_c(&raw);
        assert_eq!(info.flags.bits(), FactoryFlags::UNICODE.bits() | (1 << 9));
        assert!(info.unicode_strings());
    }

    fn with_flags(flags: FactoryFlags) -> FactoryInfo {
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
            (FactoryFlags::CLASSES_DISCARDABLE, (true, false, false)),
            (
                FactoryFlags::COMPONENT_NON_DISCARDABLE,
                (false, true, false),
            ),
            (FactoryFlags::UNICODE, (false, false, true)),
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
        let both = with_flags(FactoryFlags::CLASSES_DISCARDABLE | FactoryFlags::UNICODE);
        assert!(both.classes_discardable());
        assert!(both.unicode_strings());

        assert!(!with_flags(FactoryFlags::UNICODE).classes_discardable());
    }

    /// No flags set means no flag reads as set — pins that the accessors do not
    /// answer from a default.
    #[test]
    fn an_empty_flag_field_reports_nothing_set() {
        let none = with_flags(FactoryFlags::empty());
        assert!(!none.classes_discardable());
        assert!(!none.component_non_discardable());
        assert!(!none.unicode_strings());
    }

    /// The bit values are the ones the SDK defines (`ipluginbase.h:65-83`).
    ///
    /// Pinned because everything above reads by mask: were a constant ever
    /// wrong, every assertion would still pass while silently matching a
    /// different flag. `1 << 1` (`kLicenseCheck`, deprecated) and `1 << 2`
    /// (unassigned) are deliberately absent from `FactoryFlags`.
    #[test]
    fn the_factory_flag_bits_are_the_ones_the_sdk_defines() {
        assert_eq!(FactoryFlags::CLASSES_DISCARDABLE.bits(), 1 << 0);
        assert_eq!(FactoryFlags::COMPONENT_NON_DISCARDABLE.bits(), 1 << 3);
        assert_eq!(FactoryFlags::UNICODE.bits(), 1 << 4);
    }
}
