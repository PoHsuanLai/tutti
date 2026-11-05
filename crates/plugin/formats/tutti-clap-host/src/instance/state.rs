//! Plugin state save/load and preset loading.

use super::ClapLoaded;
use crate::error::{ClapError, Result};
use crate::host::{InputStream, OutputStream};
use crate::types::StateContext;
use std::path::Path;
use tutti_clap_ffi::{CallError, Extension};

impl ClapLoaded {
    /// Serializes the plugin's state to bytes via `CLAP_EXT_STATE`.
    ///
    /// # Errors
    /// [`ClapError::StateError`] if the plugin does not implement state or
    /// its `save` callback returns failure.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if called from a thread other than the one that
    /// loaded the plugin.
    pub(crate) fn read_state(&self) -> Result<Vec<u8>> {
        self.assert_main_thread();
        let mut stream = OutputStream::new();
        match self.plugin.state_save(&mut stream) {
            Ok(()) => Ok(stream.into_data()),
            Err(CallError::NoExtension) => {
                Err(ClapError::StateError("No state extension".to_string()))
            }
            Err(CallError::NoFunction) => {
                Err(ClapError::StateError("No save function".to_string()))
            }
            Err(CallError::Failed | CallError::WrongState) => {
                Err(ClapError::StateError("Save failed".to_string()))
            }
        }
    }

    /// Restores plugin state from bytes previously returned by [`Control::save_state`](tutti_plugin_types::Control::save_state).
    /// Empty slices are treated as a no-op.
    ///
    /// # Errors
    /// [`ClapError::StateError`] if the plugin does not implement state or
    /// its `load` callback rejects the data.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if called from a thread other than the one that
    /// loaded the plugin.
    pub(crate) fn write_state(&mut self, data: &[u8]) -> Result<()> {
        self.assert_main_thread();
        if data.is_empty() {
            return Ok(());
        }
        let mut stream = InputStream::new(data);
        match self.plugin.state_load(&mut stream) {
            Ok(()) => Ok(()),
            Err(CallError::NoExtension) => {
                Err(ClapError::StateError("No state extension".to_string()))
            }
            Err(CallError::NoFunction) => {
                Err(ClapError::StateError("No load function".to_string()))
            }
            Err(CallError::Failed | CallError::WrongState) => {
                Err(ClapError::StateError("Load failed".to_string()))
            }
        }
    }

    /// Saves state, telling the plugin whether it is being saved for a
    /// preset, project, or duplicate.
    ///
    /// Falls back to [`Control::save_state`](tutti_plugin_types::Control::save_state) **only** when the plugin does not
    /// implement `CLAP_EXT_STATE_CONTEXT` (or implements it without a `save`
    /// entry point). A plugin that implements it and returns `false` has
    /// *refused* the save, and that refusal is reported.
    ///
    /// # Errors
    /// [`ClapError::StateError`] if the plugin implements the extension and its
    /// context-aware `save` fails, or — via the fallback — if the plain
    /// `CLAP_EXT_STATE` save fails.
    ///
    /// `save` has no third "I don't handle this context" value — a plugin
    /// declines contexts by not exposing the extension — so `false` is a hard
    /// failure, not a fallback trigger. Falling through on `false` would
    /// silently answer a *preset* request with a project-context blob tagged
    /// `Ok`; the two legitimately differ, so that writes the wrong bytes into
    /// a `.preset` and surfaces only when someone loads it.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if called from a thread other than the one that
    /// loaded the plugin.
    pub fn state_with_context(&self, context: StateContext) -> Result<Vec<u8>> {
        self.assert_main_thread();
        let mut stream = OutputStream::new();
        match self
            .plugin
            .state_save_with_context(&mut stream, context.into())
        {
            Ok(()) => Ok(stream.into_data()),
            Err(CallError::Failed | CallError::WrongState) => Err(ClapError::StateError(format!(
                "Context-aware save failed for {context:?}"
            ))),
            // A plugin without the context extension, or without its `save`,
            // is asked for its plain state instead.
            Err(CallError::NoExtension | CallError::NoFunction) => self.read_state(),
        }
    }

    /// Loads state with a specific [`StateContext`].
    ///
    /// Falls back to [`Control::load_state`](tutti_plugin_types::Control::load_state) **only** when the plugin does not
    /// implement `CLAP_EXT_STATE_CONTEXT` (or implements it without a `load`
    /// entry point). Empty slices are treated as a no-op.
    ///
    /// # Errors
    /// [`ClapError::StateError`] if the plugin implements the extension and its
    /// context-aware `load` rejects the data, or — via the fallback — if the
    /// plain `CLAP_EXT_STATE` load rejects it.
    ///
    /// A rejection is likewise not a fallback trigger (see
    /// [`Self::state_with_context`]): retrying a rejected blob through the
    /// context-free `load` either gets `Ok` on state the plugin said was wrong
    /// for this context, or an error naming the wrong entry point.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if called from a thread other than the one that
    /// loaded the plugin.
    pub fn set_state_with_context(&mut self, data: &[u8], context: StateContext) -> Result<()> {
        self.assert_main_thread();
        if data.is_empty() {
            return Ok(());
        }
        let mut stream = InputStream::new(data);
        match self
            .plugin
            .state_load_with_context(&mut stream, context.into())
        {
            Ok(()) => Ok(()),
            Err(CallError::Failed | CallError::WrongState) => Err(ClapError::StateError(format!(
                "Context-aware load failed for {context:?}"
            ))),
            Err(CallError::NoExtension | CallError::NoFunction) => self.write_state(data),
        }
    }

    /// Whether the plugin implements `CLAP_EXT_STATE_CONTEXT`.
    #[must_use]
    pub fn supports_state_context(&self) -> bool {
        self.plugin.supports(Extension::StateContext)
    }

    /// Whether the plugin implements `CLAP_EXT_PRESET_LOAD`, i.e. whether
    /// [`load_preset_file`](Self::load_preset_file) can succeed.
    ///
    /// Says nothing about *enumerating* presets: CLAP puts discovery in a
    /// separate factory-level extension this host does not bind, so a plugin
    /// answering `true` here can still only be pointed at a path the host
    /// already knows.
    #[must_use]
    pub fn supports_preset_load(&self) -> bool {
        self.plugin.supports(Extension::PresetLoad)
    }

    /// Asks the plugin to load a preset from the file at `path` via
    /// `CLAP_EXT_PRESET_LOAD`.
    ///
    /// # Errors
    /// [`ClapError::StateError`] if the plugin doesn't implement preset
    /// loading, the path isn't valid UTF-8, or the plugin rejects the load.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if called from a thread other than the one that
    /// loaded the plugin.
    pub fn load_preset_file(&mut self, path: impl AsRef<Path>) -> Result<()> {
        self.assert_main_thread();
        match self.plugin.preset_load_file(path.as_ref()) {
            Ok(()) => Ok(()),
            Err(CallError::NoExtension) => Err(ClapError::StateError(
                "No preset-load extension".to_string(),
            )),
            Err(CallError::NoFunction) => Err(ClapError::StateError(
                "No from_location function".to_string(),
            )),
            Err(CallError::Failed | CallError::WrongState) => {
                Err(ClapError::StateError("Preset load failed".to_string()))
            }
        }
    }
}
