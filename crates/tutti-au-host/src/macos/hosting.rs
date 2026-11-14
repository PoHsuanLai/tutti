//! Where AU runs: in the host's process, and served from another one with an
//! editor-only instance here.

use std::path::Path;

use tutti_plugin_types::{
    InProcess, Load, MainThreadMarker, MainWorkHandle, Origin, OutOfProcess, PluginResult,
};

use crate::editor_instance::AuEditorInstance;
use crate::instance::AuLoaded;

impl InProcess for AuLoaded {
    fn load_in_process(
        main: MainThreadMarker,
        work: &MainWorkHandle,
        path: &Path,
        load: &Load,
    ) -> PluginResult<Self> {
        AuLoaded::load(main, work, path, load)
    }

    fn origin(&self) -> &Origin {
        &self.origin
    }

    fn main_work(&self) -> &MainWorkHandle {
        &self.work
    }
}

impl OutOfProcess for AuLoaded {
    type Editor = AuEditorInstance;

    fn load_served(
        main: MainThreadMarker,
        work: &MainWorkHandle,
        path: &Path,
        load: &Load,
    ) -> PluginResult<Self> {
        Self::load_in_process(main, work, path, load)
    }

    fn load_editor(
        main: MainThreadMarker,
        work: &MainWorkHandle,
        path: &Path,
        load: &Load,
    ) -> PluginResult<AuEditorInstance> {
        AuEditorInstance::load(main, work, path, load)
    }
}
