//! Where VST2 runs: in the host's process only (design 021 §4; the reason is on
//! [`Vst2Loaded`]).

use std::path::Path;

use tutti_plugin_types::bundle::{ModuleKind, native_module_path};
use tutti_plugin_types::{InProcess, Load, LoadStage, MainThread, PluginError, PluginResult};

use crate::control::plugin_error;
use crate::loaded::Vst2Loaded;

impl InProcess for Vst2Loaded {
    /// # Errors
    ///
    /// Also refuses a [`Load::plugin`] choice: a VST2 file holds one plugin.
    fn load_in_process(main: &MainThread, path: &Path, load: &Load) -> PluginResult<Self> {
        if let Some(id) = load.chosen() {
            return Err(PluginError::Load {
                stage: LoadStage::Opening,
                reason: format!("a VST2 file holds one plugin; cannot choose `{id}`"),
            });
        }
        let module = native_module_path(path, ModuleKind::Vst2)?;
        Vst2Loaded::load(main, &module).map_err(plugin_error)
    }
}
