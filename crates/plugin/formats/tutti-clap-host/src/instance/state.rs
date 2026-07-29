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
    pub fn state(&self) -> Result<Vec<u8>> {
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

    /// Restore plugin state from bytes previously returned by [`Self::state`].
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
    /// ## Why the refusal is not a fallback trigger
    ///
    /// `clap_plugin_state_context.save` returns `bool` with the same meaning as
    /// `clap_plugin_state.save`: `true` on success, `false` on failure. It has
    /// no third "I don't handle this context" value — a plugin that does not
    /// handle contexts declines by not exposing the extension at all. So
    /// `false` is a hard failure, exactly as it is for the plain state path,
    /// which this crate already treats as [`ClapError::StateError`].
    ///
    /// This method used to fall through to [`Self::state`] on `false` as well,
    /// which silently downgraded the caller's request: it asked for a blob
    /// saved *for a preset* and got one saved for the project context, tagged
    /// `Ok`. Preset and project blobs legitimately differ (a preset omits
    /// project-scoped bindings), so the substitution is not benign — it writes
    /// the wrong bytes into a `.preset` file and only shows up when someone
    /// loads it. That is the same "a `no` was read as a `not applicable`"
    /// mistake as the `activate` bug in [`super::lifecycle`].
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
        self.state()
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
    /// ## Why the rejection is not a fallback trigger
    ///
    /// The load side had the same shape as the save side and the same bug, with
    /// a worse failure mode: retrying a *rejected* blob through the
    /// context-free `load` asks the plugin to swallow bytes it just refused. If
    /// it accepts them the caller gets `Ok` on state the plugin told us was
    /// wrong for this context; if it refuses again the error names the wrong
    /// entry point. See [`Self::state_with_context`] for why `false` here is a
    /// failure and not a "not applicable".
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
