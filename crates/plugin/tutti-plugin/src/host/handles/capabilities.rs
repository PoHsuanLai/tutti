//! Granular host-side plugin-control capability traits.
//!
//! These replace the former `ControlBackend` god-trait: instead of one ~13-method
//! trait every backend implemented in full (faking the capabilities it lacked with
//! no-op/error stubs), a backend implements exactly the capabilities it honors, and
//! a consumer depends only on the capability it uses.
//!
//! **Naming.** These are the *host-side* mirror of the loader-side `Plugin*`
//! capability traits in `tutti-plugin-types` (`PluginParams`/`PluginState`/
//! `PluginEditorHost`), which live *inside the subprocess*. The two rows are named
//! apart because they are different objects doing the same job on opposite sides of
//! the IPC boundary — exactly as the host-side `AudioUnit` node mirrors the
//! loader-side `PluginAudio`. So the host-side control traits take the `Host*`
//! prefix.
//!
//! All are object-safe (`&self`, no generics on the trait methods, `Send + Sync`):
//! a `PluginHandle` stores them as `Arc<dyn …>`.
//!
//! - [`HostParams`] and [`HostState`] are **always present** — every backend
//!   implements them.
//! - [`HostEditor`] is **optional**: a backend with no embeddable editor simply
//!   does not implement it, so `PluginHandle::editor()` returns `None`. Absence
//!   is type-level; there is no `open_editor → Err(GuiNotSupported)` stub to
//!   fake it.
//! - `is_crashed` is a single-method concern folded onto the always-present set via
//!   [`HostParams`], rather than a standalone trait too thin to stand alone.

use std::ffi::c_void;

use crate::error::EditorError;
use crate::protocol::{AutomationMode, ParameterInfo};
use crate::util::window::{EditorCapabilities, EditorSize};

/// Parameter catalog, live-value read, and imperative value write — plus the
/// backend's liveness (`is_crashed`), folded here rather than in a one-method
/// trait of its own.
///
/// Method names say *what kind of thing* each deals in: `_descriptors` is the
/// static metadata catalog; `_value` / `set_..._value` is the live number. This
/// is deliberately NOT the automation/modulation path — sample-accurate parameter
/// automation is a per-block `BlockInput` producer installed on the audio node
/// (`set_param_automation_source`), never a method here.
pub trait HostParams: Send + Sync {
    /// The static parameter catalog (id, name, range, flags). `None` if the
    /// backend cannot enumerate parameters.
    fn parameter_descriptors(&self) -> Option<Vec<ParameterInfo>>;

    /// The plugin's current live value for one parameter. `None` if unavailable.
    fn parameter_value(&self, id: u32) -> Option<f32>;

    /// Write one parameter value (a UI knob poke / initial preset value).
    /// Main-thread; fire-and-forget. In-process backends `try_lock` internally so
    /// a shared handle can never block the audio thread.
    fn set_parameter_value(&self, id: u32, value: f32);

    /// `true` if the underlying plugin is gone (subprocess crashed). In-process
    /// backends never return `true` — a crash takes the host down with it.
    fn is_crashed(&self) -> bool;
}

/// Opaque preset-chunk save / load. Raw `Vec<u8>` — the bytes are the plugin's
/// business (the same shape the loader-side `PluginState::get_state` returns).
pub trait HostState: Send + Sync {
    fn save_state(&self) -> Option<Vec<u8>>;
    fn load_state(&self, data: &[u8]);
}

/// Editor / GUI hosting — **optional**. A backend implements this only if it can
/// embed the plugin's editor; a headless one leaves it unimplemented, so its
/// plugins report `PluginHandle::editor() == None` rather than erroring at open
/// time.
///
/// `parent` is a raw platform window pointer (not a generic `HasWindowHandle`) so
/// the trait stays object-safe; the ergonomic `HasWindowHandle` entry lives on
/// [`PluginHandle::open_editor`](super::control_handle::PluginHandle::open_editor).
pub trait HostEditor: Send + Sync {
    fn open_editor(&self, parent: *mut c_void) -> Result<EditorSize, EditorError>;

    fn close_editor(&self);

    /// Call periodically (~30 Hz) while the editor is open.
    fn editor_idle(&self);

    fn editor_capabilities(&self) -> EditorCapabilities {
        EditorCapabilities::default()
    }

    /// Returns the snapped/clamped size the plugin actually applied.
    fn set_editor_size(&self, _requested: EditorSize) -> Result<EditorSize, EditorError> {
        Err(EditorError::PluginError(
            "set_editor_size not supported".into(),
        ))
    }

    fn poll_editor_resize_request(&self) -> Option<EditorSize> {
        None
    }
}

/// Host → plugin automation-state advisory — **optional**, Direction C-in.
///
/// The host announces what it is doing with automation (reading / writing /
/// neither) so the plugin's editor can show UI feedback (a glowing knob ring
/// while the host records automation onto it). The plugin does nothing audible
/// with this — it is purely cosmetic. A backend implements this only if the
/// underlying format supports the advisory (VST3 `IAutomationState`); others do
/// not implement it, so [`PluginHandle::automation_state`] returns `None` — no
/// stub.
///
/// [`PluginHandle::automation_state`]: super::control_handle::PluginHandle::automation_state
pub trait HostAutomationState: Send + Sync {
    /// Announce the host's current automation mode to the plugin. **Global** (no
    /// `param_id`) — the only wired sink (VST3 `IAutomationState`) is global.
    ///
    /// Fallible: the call can fail because the backend is gone, there is no open
    /// editor to deliver the feedback to, or the format's automation interface is
    /// absent. `Ok(())` means *delivered / accepted*, not that the plugin visibly
    /// reacted (no format confirms the reaction).
    fn set_automation_mode(&self, mode: AutomationMode) -> Result<(), EditorError>;
}

/// Compile-time guard that all four control capabilities stay **object-safe** —
/// `PluginHandle` stores each as `Arc<dyn …>`, so a regression that breaks
/// dyn-compatibility (e.g. adding a generic method) must fail here, not at a
/// distant call site.
#[allow(dead_code)]
fn _assert_object_safe(
    _p: &dyn HostParams,
    _s: &dyn HostState,
    _e: &dyn HostEditor,
    _a: &dyn HostAutomationState,
) {
}
