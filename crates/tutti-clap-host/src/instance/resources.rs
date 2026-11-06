//! Resource-directory methods from the draft `CLAP_EXT_RESOURCE_DIRECTORY`.

use std::path::{Path, PathBuf};

use super::ClapLoaded;

impl ClapLoaded {
    /// Tells the plugin where to read/write its resources. `is_shared`
    /// selects between the shared (project-level) and private (preset-level)
    /// directory.
    pub fn resource_set_directory(&self, path: impl AsRef<Path>, is_shared: bool) {
        self.plugin.resource_set_directory(path.as_ref(), is_shared);
    }

    /// Asks the plugin to enumerate the resource files it currently uses.
    /// If `all` is true, include files under the shared directory as well.
    pub fn resource_collect(&self, all: bool) {
        self.plugin.resource_collect(all);
    }

    /// Number of resource files the plugin reported during the last
    /// [`Self::resource_collect`] call.
    pub fn resource_file_count(&self) -> u32 {
        self.plugin.resource_file_count()
    }

    /// Path to the resource file at `index`, or `None` if unavailable.
    pub fn resource_file_path(&self, index: u32) -> Option<PathBuf> {
        self.plugin.resource_file_path(index).map(PathBuf::from)
    }
}
