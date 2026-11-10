//! Where CLAP runs: in the host's process, and served from another one with
//! an editor-only instance here (design 021 §3).

use std::path::Path;

use tutti_plugin_types::bundle::{ModuleKind, native_module_path};
use tutti_plugin_types::{InProcess, Load, MainThread, OutOfProcess, PluginResult};

use super::ClapLoaded;
use super::editor_instance::ClapEditor;
use super::lifecycle::plugin_error;
use super::load::LoadOptions;

fn options(load: &Load) -> LoadOptions {
    match load.chosen() {
        Some(id) => LoadOptions::new().plugin_id(id),
        None => LoadOptions::new(),
    }
}

impl InProcess for ClapLoaded {
    fn load_in_process(main: &MainThread, path: &Path, load: &Load) -> PluginResult<Self> {
        let module = native_module_path(path, ModuleKind::Clap)?;
        ClapLoaded::load(main, &module, options(load)).map_err(plugin_error)
    }
}

impl OutOfProcess for ClapLoaded {
    type Editor = ClapEditor;

    fn load_served(main: &MainThread, path: &Path, load: &Load) -> PluginResult<Self> {
        Self::load_in_process(main, path, load)
    }

    fn load_editor(main: &MainThread, path: &Path, load: &Load) -> PluginResult<ClapEditor> {
        ClapEditor::load(main, path, load)
    }
}
