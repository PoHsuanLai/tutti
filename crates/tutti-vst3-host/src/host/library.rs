//! VST3 library loading and factory access.

use std::path::Path;
use std::sync::Arc;

use tutti_vst3_ffi::Module;
pub use tutti_vst3_ffi::{ClassInfo, FactoryInfo, factory_flags};

use crate::error::{LoadCause, LoadStage, Result, Vst3Error};

/// A loaded VST3 dynamic library with its `IPluginFactory` resolved.
///
/// Shared via [`Arc`] because a single bundle commonly exposes multiple plugin
/// classes, and each [`Vst3Loaded`](crate::Vst3Loaded) instance keeps its
/// originating library alive for the plugin's lifetime.
#[derive(Debug)]
pub struct Library {
    module: Arc<Module>,
}

impl Library {
    /// The module behind this library, which a plugin created from it keeps
    /// alive.
    pub(crate) fn module(&self) -> &Arc<Module> {
        &self.module
    }

    /// Loads a VST3 library from a pre-resolved path to the actual binary (the
    /// inner Mach-O / ELF / PE, not the bundle directory).
    ///
    /// # Errors
    ///
    /// Returns [`Vst3Error::LoadFailed`] if the OS cannot open the library, if
    /// the module's entry point refuses initialization, if the
    /// `GetPluginFactory` symbol is missing, or if the factory function returns
    /// null.
    pub fn load(lib_path: &Path) -> Result<Arc<Self>> {
        let module = Module::open(lib_path).map_err(|e| Vst3Error::LoadFailed {
            path: lib_path.to_path_buf(),
            stage: e.stage,
            cause: LoadCause::Module(e.reason),
        })?;
        Ok(Arc::new(Self { module }))
    }

    /// Vendor/URL/email and the factory flags. `None` if the plugin rejects the
    /// `getFactoryInfo` call.
    pub fn factory_info(&self) -> Option<FactoryInfo> {
        self.module.factory_info()
    }

    /// Number of plugin classes exposed by this factory (audio processors,
    /// controllers, etc.). A single bundle may contain many.
    pub fn count_classes(&self) -> i32 {
        self.module.class_count()
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
    /// Returns [`Vst3Error::PluginError`] if the factory rejects the index.
    /// A factory that implements `IPluginFactory3` but rejects the index in
    /// `getClassInfoUnicode` still gets the ASCII attempt before erroring.
    pub fn class_info(&self, index: i32) -> Result<ClassInfo> {
        self.module
            .class_info(index)
            .map_err(|e| Vst3Error::PluginError {
                stage: LoadStage::Factory,
                code: e.code(),
            })
    }
}
