//! Backend trait abstracting how a [`PluginHandle`](crate::handles::PluginHandle)
//! reaches the live plugin.
//!
//! Two concrete impls today:
//!
//! - `PluginBridge` (in `bridge::composite`) — out-of-process audio +
//!   in-process GUI dance for VST3 / CLAP / AU. Holds the subprocess
//!   lifetime guard internally.
//! - `InProcessVst2Backend` (in `in_process::vst2::control_backend`) —
//!   single-process VST2 host where the AudioUnit and the handle share
//!   one `Arc<Mutex<tutti_vst2_host::Vst2Instance>>`.
//!
//! Each backend is responsible for keeping its own underlying plugin
//! alive — either via a subprocess `ProcessGuard` Arc or via the shared
//! Mutex — so [`PluginHandle`] doesn't need format-aware lifetime
//! plumbing.

use std::ffi::c_void;

use crate::error::EditorError;
use crate::protocol::ParameterInfo;
use crate::window::{EditorCapabilities, EditorSize};

/// Format-agnostic control-surface a `PluginHandle` dispatches to.
///
/// All methods are callable from the main thread. `set_parameter_rt` is
/// the lone audio-thread-callable entry — implementations must be
/// allocation-free and non-blocking on that path. The other methods may
/// allocate / lock / do IPC.
///
/// Public so out-of-crate in-process loaders (e.g. `tutti-wasm-plugin`)
/// can build a [`PluginHandle`](crate::handles::PluginHandle) over their
/// own backend via [`PluginHandle::from_backend`](crate::handles::PluginHandle::from_backend).
pub trait ControlBackend: Send + Sync {
    fn open_editor(&self, parent_ptr: *mut c_void) -> std::result::Result<EditorSize, EditorError>;

    fn close_editor(&self);

    fn editor_idle(&self);

    fn save_state(&self) -> Option<Vec<u8>>;

    fn load_state(&self, data: &[u8]);

    fn parameters(&self) -> Option<Vec<ParameterInfo>>;

    fn parameter(&self, id: u32) -> Option<f32>;

    /// RT-safe parameter write. Must be allocation-free and never block
    /// the audio thread.
    fn set_parameter_rt(&self, id: u32, value: f32);

    /// Push the host automation state to the plugin (VST3 `IAutomationState`).
    /// Fire-and-forget; default no-op so backends without the concept don't
    /// need to implement it.
    fn set_automation_state_rt(&self, _state: i32) {}

    /// `true` if the underlying plugin is gone (subprocess crashed, in-
    /// process backend never returns true since a crash takes the host
    /// down with it).
    fn is_crashed(&self) -> bool;

    fn editor_capabilities(&self) -> EditorCapabilities {
        EditorCapabilities::default()
    }

    /// Returns the snapped size the plugin actually applied.
    fn set_editor_size(
        &self,
        _requested: EditorSize,
    ) -> std::result::Result<EditorSize, EditorError> {
        Err(EditorError::PluginError(
            "set_editor_size not supported".into(),
        ))
    }

    fn poll_editor_resize_request(&self) -> Option<EditorSize> {
        None
    }
}
