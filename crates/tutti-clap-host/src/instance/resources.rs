//! Resource-directory methods from the draft `CLAP_EXT_RESOURCE_DIRECTORY`.

use std::path::{Path, PathBuf};
use tutti_plugin_types::MainThread;

use super::ClapLoaded;

impl ClapLoaded {
    /// Tells the plugin where to read/write its resources. `is_shared`
    /// selects between the shared (project-level) and private (preset-level)
    /// directory.
    pub fn resource_set_directory(
        &self,
        main: &MainThread,
        path: impl AsRef<Path>,
        is_shared: bool,
    ) {
        self.plugin
            .resource_set_directory(main, path.as_ref(), is_shared);
    }

    /// Asks the plugin to enumerate the resource files it currently uses.
    /// If `all` is true, include files under the shared directory as well.
    pub fn resource_collect(&self, main: &MainThread, all: bool) {
        self.plugin.resource_collect(main, all);
    }

    /// Number of resource files the plugin reported during the last
    /// [`Self::resource_collect`] call.
    pub fn resource_file_count(&self, main: &MainThread) -> u32 {
        self.plugin.resource_file_count(main)
    }

    /// Path to the resource file at `index`, or `None` if unavailable.
    pub fn resource_file_path(&self, main: &MainThread, index: u32) -> Option<PathBuf> {
        self.plugin
            .resource_file_path(main, index)
            .map(PathBuf::from)
    }
}
