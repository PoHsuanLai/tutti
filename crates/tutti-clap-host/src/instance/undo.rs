//! Undo / redo support from the draft `CLAP_EXT_UNDO` extensions.

use super::ext;
use super::ClapInstance;
use crate::types::UndoDeltaProperties;
use clap_sys::ext::draft::undo::clap_undo_delta_properties;

impl ClapInstance {
    /// Query the plugin's undo delta capabilities (whether it produces
    /// deltas, whether they persist across sessions, format version).
    pub fn undo_get_delta_properties(&self) -> Option<UndoDeltaProperties> {
        let ext = unsafe { ext::opt(self.extensions.undo.delta) }?;
        let get_fn = ext.get_delta_properties?;
        let mut props: clap_undo_delta_properties = unsafe { std::mem::zeroed() };
        unsafe { get_fn(self.plugin.as_ptr(), &mut props) };
        Some(UndoDeltaProperties {
            has_delta: props.has_delta,
            are_deltas_persistent: props.are_deltas_persistent,
            format_version: props.format_version,
        })
    }

    /// Ask whether the plugin can decode undo deltas of the given format
    /// version. Useful before restoring deltas saved by an older release.
    pub fn undo_can_use_format_version(&self, version: u32) -> bool {
        let Some(ext) = (unsafe { ext::opt(self.extensions.undo.delta) }) else {
            return false;
        };
        ext.can_use_delta_format_version
            .map(|f| unsafe { f(self.plugin.as_ptr(), version) })
            .unwrap_or(false)
    }

    /// Undo a previous change by applying its delta. Returns whether the
    /// plugin accepted the delta.
    pub fn undo_apply_delta(&mut self, format_version: u32, delta: &[u8]) -> bool {
        let Some(ext) = (unsafe { ext::opt(self.extensions.undo.delta) }) else {
            return false;
        };
        ext.undo
            .map(|f| unsafe {
                f(
                    self.plugin.as_ptr(),
                    format_version,
                    delta.as_ptr() as *const _,
                    delta.len(),
                )
            })
            .unwrap_or(false)
    }

    /// Redo a previously-undone change.
    pub fn redo_apply_delta(&mut self, format_version: u32, delta: &[u8]) -> bool {
        let Some(ext) = (unsafe { ext::opt(self.extensions.undo.delta) }) else {
            return false;
        };
        ext.redo
            .map(|f| unsafe {
                f(
                    self.plugin.as_ptr(),
                    format_version,
                    delta.as_ptr() as *const _,
                    delta.len(),
                )
            })
            .unwrap_or(false)
    }

    /// Update the plugin's UI to reflect whether the host currently has an
    /// undoable action.
    pub fn undo_set_can_undo(&self, can_undo: bool) {
        let Some(ext) = (unsafe { ext::opt(self.extensions.undo.context) }) else {
            return;
        };
        if let Some(f) = ext.set_can_undo {
            unsafe { f(self.plugin.as_ptr(), can_undo) };
        }
    }

    /// Update the plugin's UI to reflect whether redo is currently available.
    pub fn undo_set_can_redo(&self, can_redo: bool) {
        let Some(ext) = (unsafe { ext::opt(self.extensions.undo.context) }) else {
            return;
        };
        if let Some(f) = ext.set_can_redo {
            unsafe { f(self.plugin.as_ptr(), can_redo) };
        }
    }

    /// Publish the human-readable name of the currently available undo step.
    pub fn undo_set_undo_name(&self, name: &str) {
        let Some(ext) = (unsafe { ext::opt(self.extensions.undo.context) }) else {
            return;
        };
        if let Some(f) = ext.set_undo_name {
            if let Ok(cstr) = std::ffi::CString::new(name) {
                unsafe { f(self.plugin.as_ptr(), cstr.as_ptr()) };
            }
        }
    }

    /// Publish the human-readable name of the currently available redo step.
    pub fn undo_set_redo_name(&self, name: &str) {
        let Some(ext) = (unsafe { ext::opt(self.extensions.undo.context) }) else {
            return;
        };
        if let Some(f) = ext.set_redo_name {
            if let Ok(cstr) = std::ffi::CString::new(name) {
                unsafe { f(self.plugin.as_ptr(), cstr.as_ptr()) };
            }
        }
    }
}
