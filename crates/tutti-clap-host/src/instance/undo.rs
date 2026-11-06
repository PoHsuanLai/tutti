//! Undo / redo support from the draft `CLAP_EXT_UNDO` extensions.

use super::ClapLoaded;
use crate::types::UndoDeltaProperties;

impl ClapLoaded {
    /// Queries the plugin's undo delta capabilities (whether it produces
    /// deltas, whether they persist across sessions, format version).
    pub fn undo_get_delta_properties(&self) -> Option<UndoDeltaProperties> {
        let props = self.plugin.undo_delta_properties()?;
        Some(UndoDeltaProperties {
            has_delta: props.has_delta,
            are_deltas_persistent: props.are_deltas_persistent,
            format_version: props.format_version,
        })
    }

    /// Asks whether the plugin can decode undo deltas of the given format
    /// version. Useful before restoring deltas saved by an older release.
    pub fn undo_can_use_format_version(&self, version: u32) -> bool {
        self.plugin.undo_can_use_format_version(version)
    }

    /// Undo a previous change by applying its delta. Returns whether the
    /// plugin accepted the delta.
    pub fn undo_apply_delta(&mut self, format_version: u32, delta: &[u8]) -> bool {
        self.plugin.undo_apply_delta(format_version, delta)
    }

    /// Redoes a previously undone change.
    pub fn redo_apply_delta(&mut self, format_version: u32, delta: &[u8]) -> bool {
        self.plugin.redo_apply_delta(format_version, delta)
    }

    /// Updates the plugin's UI to reflect whether the host currently has an
    /// undoable action.
    pub fn undo_set_can_undo(&self, can_undo: bool) {
        self.plugin.undo_set_can_undo(can_undo);
    }

    /// Updates the plugin's UI to reflect whether redo is currently available.
    pub fn undo_set_can_redo(&self, can_redo: bool) {
        self.plugin.undo_set_can_redo(can_redo);
    }

    /// Publishes the human-readable name of the currently available undo step.
    pub fn undo_set_undo_name(&self, name: &str) {
        self.plugin.undo_set_undo_name(name);
    }

    /// Publishes the human-readable name of the currently available redo step.
    pub fn undo_set_redo_name(&self, name: &str) {
        self.plugin.undo_set_redo_name(name);
    }
}
