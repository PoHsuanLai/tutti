//! Where AU runs: in the host's process, and served from another one with an
//! editor-only instance here (design 021 §3).

use std::path::Path;

use tutti_plugin_types::{InProcess, Load, MainThread, Origin, OutOfProcess, PluginResult};

use crate::editor_instance::AuEditorInstance;
use crate::instance::AuLoaded;

impl InProcess for AuLoaded {
    fn load_in_process(main: &MainThread, path: &Path, load: &Load) -> PluginResult<Self> {
        AuLoaded::load(main, path, load)
    }

    fn origin(&self) -> &Origin {
        &self.origin
    }
}

impl OutOfProcess for AuLoaded {
    type Editor = AuEditorInstance;

    fn load_served(main: &MainThread, path: &Path, load: &Load) -> PluginResult<Self> {
        Self::load_in_process(main, path, load)
    }

    fn load_editor(main: &MainThread, path: &Path, load: &Load) -> PluginResult<AuEditorInstance> {
        AuEditorInstance::load(main, path, load)
    }
}
