//! Plugin state save/load and preset loading.

use super::ext;
use super::ClapLoaded;
use crate::error::{ClapError, Result};
use crate::host::{InputStream, OutputStream};
use crate::types::StateContext;
use clap_sys::factory::preset_discovery::CLAP_PRESET_DISCOVERY_LOCATION_FILE;
use std::path::Path;
use std::ptr;

impl ClapLoaded {
    /// Serialize the plugin's state to bytes via `CLAP_EXT_STATE`.
    ///
    /// # Errors
    /// [`ClapError::StateError`] if the plugin does not implement state or
    /// its `save` callback returns failure.
    pub fn get_state(&self) -> Result<Vec<u8>> {
        self.assert_main_thread();
        let state_ext = unsafe { ext::opt(self.extensions.state.state) }
            .ok_or_else(|| ClapError::StateError("No state extension".to_string()))?;
        let save_fn = state_ext
            .save
            .ok_or_else(|| ClapError::StateError("No save function".to_string()))?;

        let mut stream = OutputStream::new();
        if !unsafe { save_fn(self.plugin.as_ptr(), stream.as_raw()) } {
            return Err(ClapError::StateError("Save failed".to_string()));
        }

        Ok(stream.into_data())
    }

    /// Restore plugin state from bytes previously returned by [`Self::get_state`].
    /// Empty slices are treated as a no-op.
    ///
    /// # Errors
    /// [`ClapError::StateError`] if the plugin does not implement state or
    /// its `load` callback rejects the data.
    pub fn set_state(&mut self, data: &[u8]) -> Result<()> {
        self.assert_main_thread();
        if data.is_empty() {
            return Ok(());
        }

        let state_ext = unsafe { ext::opt(self.extensions.state.state) }
            .ok_or_else(|| ClapError::StateError("No state extension".to_string()))?;
        let load_fn = state_ext
            .load
            .ok_or_else(|| ClapError::StateError("No load function".to_string()))?;

        let mut stream = InputStream::new(data);
        if !unsafe { load_fn(self.plugin.as_ptr(), stream.as_raw()) } {
            return Err(ClapError::StateError("Load failed".to_string()));
        }

        Ok(())
    }

    /// Save state, telling the plugin whether it is being saved for a
    /// preset, project, or duplicate.
    ///
    /// Falls back to [`Self::state`] **only** when the plugin does not
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
    pub fn state_with_context(&self, context: StateContext) -> Result<Vec<u8>> {
        self.assert_main_thread();
        if let Some(ext) = unsafe { ext::opt(self.extensions.state.context) } {
            if let Some(save_fn) = ext.save {
                let mut stream = OutputStream::new();
                if !unsafe { save_fn(self.plugin.as_ptr(), stream.as_raw(), context.into()) } {
                    return Err(ClapError::StateError(format!(
                        "Context-aware save failed for {context:?}"
                    )));
                }
                return Ok(stream.into_data());
            }
        }
        self.get_state()
    }

    /// Load state with a specific [`StateContext`].
    ///
    /// Falls back to [`Self::set_state`] **only** when the plugin does not
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
    pub fn set_state_with_context(&mut self, data: &[u8], context: StateContext) -> Result<()> {
        self.assert_main_thread();
        if data.is_empty() {
            return Ok(());
        }
        if let Some(ext) = unsafe { ext::opt(self.extensions.state.context) } {
            if let Some(load_fn) = ext.load {
                let mut stream = InputStream::new(data);
                if !unsafe { load_fn(self.plugin.as_ptr(), stream.as_raw(), context.into()) } {
                    return Err(ClapError::StateError(format!(
                        "Context-aware load failed for {context:?}"
                    )));
                }
                return Ok(());
            }
        }
        self.set_state(data)
    }

    /// Whether the plugin implements `CLAP_EXT_STATE_CONTEXT`.
    pub fn supports_state_context(&self) -> bool {
        !self.extensions.state.context.is_null()
    }

    /// Whether the plugin implements `CLAP_EXT_PRESET_LOAD`, i.e. whether
    /// [`load_preset`](Self::load_preset) can succeed.
    ///
    /// Says nothing about *enumerating* presets: CLAP puts discovery in a
    /// separate factory-level extension this host does not bind, so a plugin
    /// answering `true` here can still only be pointed at a path the host
    /// already knows.
    pub fn supports_preset_load(&self) -> bool {
        !self.extensions.state.preset_load.is_null()
    }

    /// Ask the plugin to load a preset from the file at `path` via
    /// `CLAP_EXT_PRESET_LOAD`.
    ///
    /// # Errors
    /// [`ClapError::StateError`] if the plugin doesn't implement preset
    /// loading, the path isn't valid UTF-8, or the plugin rejects the load.
    pub fn load_preset(&mut self, path: &Path) -> Result<()> {
        self.assert_main_thread();
        let ext = unsafe { ext::opt(self.extensions.state.preset_load) }
            .ok_or_else(|| ClapError::StateError("No preset-load extension".to_string()))?;
        let from_location_fn = ext
            .from_location
            .ok_or_else(|| ClapError::StateError("No from_location function".to_string()))?;
        let location = std::ffi::CString::new(path.to_string_lossy().as_ref())
            .map_err(|e| ClapError::StateError(format!("Invalid path: {e}")))?;
        if unsafe {
            from_location_fn(
                self.plugin.as_ptr(),
                CLAP_PRESET_DISCOVERY_LOCATION_FILE,
                location.as_ptr(),
                ptr::null(),
            )
        } {
            Ok(())
        } else {
            Err(ClapError::StateError("Preset load failed".to_string()))
        }
    }
}
