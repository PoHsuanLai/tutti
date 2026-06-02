//! Resource-directory methods from the draft `CLAP_EXT_RESOURCE_DIRECTORY`.

use super::ext;
use super::ClapInstance;
use crate::cstr_to_string;

impl ClapInstance {
    /// Tell the plugin where to read/write its resources. `is_shared`
    /// selects between the shared (project-level) and private (preset-level)
    /// directory.
    pub fn resource_set_directory(&self, path: &str, is_shared: bool) {
        let Some(ext) = (unsafe { ext::opt(self.extensions.system.resource_directory) }) else {
            return;
        };
        if let Some(f) = ext.set_directory {
            if let Ok(cstr) = std::ffi::CString::new(path) {
                unsafe { f(self.plugin.as_ptr(), cstr.as_ptr(), is_shared) };
            }
        }
    }

    /// Ask the plugin to enumerate the resource files it currently uses.
    /// If `all` is true, include files under the shared directory as well.
    pub fn resource_collect(&self, all: bool) {
        let Some(ext) = (unsafe { ext::opt(self.extensions.system.resource_directory) }) else {
            return;
        };
        if let Some(f) = ext.collect {
            unsafe { f(self.plugin.as_ptr(), all) };
        }
    }

    /// Number of resource files the plugin reported during the last
    /// [`Self::resource_collect`] call.
    pub fn resource_files_count(&self) -> u32 {
        let Some(ext) = (unsafe { ext::opt(self.extensions.system.resource_directory) }) else {
            return 0;
        };
        ext.get_files_count
            .map(|f| unsafe { f(self.plugin.as_ptr()) })
            .unwrap_or(0)
    }

    /// Path to the resource file at `index`, or `None` if unavailable.
    pub fn resource_get_file_path(&self, index: u32) -> Option<String> {
        let ext = unsafe { ext::opt(self.extensions.system.resource_directory) }?;
        let get_fn = ext.get_file_path?;
        let mut buf = [0i8; 4096];
        let result = unsafe {
            get_fn(
                self.plugin.as_ptr(),
                index,
                buf.as_mut_ptr(),
                buf.len() as u32,
            )
        };
        if result < 0 {
            return None;
        }
        Some(unsafe { cstr_to_string(buf.as_ptr()) })
    }
}
