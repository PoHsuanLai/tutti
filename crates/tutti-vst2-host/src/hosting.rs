//! Where VST2 runs: in the host's process only (design 021 §4; the reason is on
//! [`Vst2Loaded`]).

use std::path::Path;

use tutti_plugin_types::bundle::{ModuleKind, native_module_path};
use tutti_plugin_types::{
    InProcess, Load, LoadStage, MainThread, Origin, PluginError, PluginResult,
};

use crate::control::plugin_error;
use crate::loaded::Vst2Loaded;

impl InProcess for Vst2Loaded {
    /// # Errors
    ///
    /// Also refuses a [`Load::plugin`] that is not `vst2.<uniqueID>` of a plugin the
    /// file holds: a shell's sub-plugin, or the one plugin of a file that is no shell.
    fn load_in_process(main: &MainThread, path: &Path, load: &Load) -> PluginResult<Self> {
        let module = native_module_path(path, ModuleKind::Vst2)?;
        match load.chosen() {
            None => Vst2Loaded::load(main, &module),
            Some(chosen) => {
                let id = chosen
                    .strip_prefix("vst2.")
                    .and_then(|n| n.parse::<i32>().ok())
                    .ok_or_else(|| PluginError::Load {
                        stage: LoadStage::Opening,
                        reason: format!("`{chosen}` is not a VST2 plugin id (`vst2.<uniqueID>`)"),
                    })?;
                Vst2Loaded::load_plugin(main, &module, id)
            }
        }
        .map_err(plugin_error)
    }

    fn origin(&self) -> &Origin {
        &self.origin
    }
}
