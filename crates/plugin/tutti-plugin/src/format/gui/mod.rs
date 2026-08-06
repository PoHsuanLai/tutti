//! In-process plugin editor.
//!
//! The plugin-server subprocess owns audio processing; the editor must
//! live in the host process because platform GUI toolkits are
//! host-process-local. Each supported format (VST3, CLAP, AU) has its
//! own [`PluginEditor`] implementation that dlopens the plugin library
//! a second time purely for the editor.

#[cfg(all(feature = "au", target_os = "macos"))]
mod au;
#[cfg(all(feature = "clap", unix))]
mod clap;
#[cfg(all(feature = "vst3", unix))]
mod vst3;

use crate::error::{BridgeError, LoadStage, Result};
use crate::protocol::ParamAddress;
use crate::util::window::{EditorCapabilities, EditorSize, WindowHandle};
use std::path::Path;

/// The host-side, in-process plugin editor.
///
/// The two-world counterpart of the subprocess-side
/// [`PluginEditorHost`](tutti_plugin_types::PluginEditorHost): this object is a
/// *second* dlopen of the plugin, editor-only, living in the host process
/// because platform GUI toolkits are host-process-local. It carries the display
/// helpers the editor needs (parameter/state push, GUI-originated param polling)
/// that the audio subprocess object does not.
///
/// Kept `pub(crate)` — its only consumer is the host `PluginBridge`
/// ([`composite`](crate::host::ipc_client::composite)); promote to `pub` if a
/// cross-crate consumer ever appears.
pub(crate) trait PluginEditor: Send {
    fn open_editor(&mut self, parent: WindowHandle) -> Result<EditorSize>;

    /// Open as a plugin-owned floating window. No parent in, no size out.
    ///
    /// Defaulted to a refusal: only CLAP has the concept, so VST3 and AU are
    /// not made to write an override that could only say this.
    fn open_floating_editor(&mut self) -> Result<()> {
        Err(BridgeError::ProtocolError(
            "this plugin format has no floating-window editor".into(),
        ))
    }

    fn close_editor(&mut self);
    fn editor_idle(&mut self);
    fn set_parameter(&mut self, id: ParamAddress, value: f64);
    fn set_state(&mut self, data: &[u8]) -> Result<()>;
    /// Poll GUI-originated parameter changes to forward to the audio bridge.
    ///
    /// Every format with an editor here is one of the three opaque-id formats
    /// (VST2's in-process editor does not go through this trait), so an impl
    /// tags what the plugin handed it rather than choosing a model.
    fn poll_gui_param_changes(&mut self) -> Vec<(ParamAddress, f32)>;

    /// Push the host [`AutomationMode`](crate::protocol::AutomationMode) to the
    /// GUI instance so the editor can update its automation UI feedback. Each
    /// format encodes the mode onto its own ABI (VST3 `IAutomationState`, …).
    /// Default no-op for formats without the concept; only the VST3 GUI overrides it.
    fn set_automation_state(&mut self, _mode: crate::protocol::AutomationMode) {}

    fn editor_capabilities(&mut self) -> EditorCapabilities {
        EditorCapabilities::default()
    }

    fn set_editor_size(&mut self, _requested: EditorSize) -> Result<EditorSize> {
        Err(BridgeError::ProtocolError(
            "set_editor_size not supported".into(),
        ))
    }

    fn poll_editor_resize_request(&mut self) -> Option<EditorSize> {
        None
    }
}

#[cfg(all(any(feature = "vst3", feature = "clap", feature = "au"), unix))]
pub(crate) fn load_gui_instance(path: &Path) -> Result<Box<dyn PluginEditor>> {
    use crate::host::discovery::{format_from_path, PluginFormat};

    let format = format_from_path(path).ok_or_else(|| BridgeError::LoadFailed {
        path: path.to_path_buf(),
        stage: LoadStage::Scanning,
        reason: format!("Unknown plugin format for: {}", path.display()),
    })?;

    match format {
        #[cfg(feature = "vst3")]
        PluginFormat::Vst3 => Ok(Box::new(vst3::Vst3GuiInstance::load(path)?)),
        #[cfg(feature = "clap")]
        PluginFormat::Clap => Ok(Box::new(clap::ClapGuiInstance::load(path)?)),
        #[cfg(all(feature = "au", target_os = "macos"))]
        PluginFormat::AudioUnit => Ok(Box::new(au::AuGuiInstance::load(path)?)),
        PluginFormat::Vst2 => Err(BridgeError::LoadFailed {
            path: path.to_path_buf(),
            stage: LoadStage::Opening,
            reason: "VST2 plugins use the in-process backend (enable the \
                     `vst2` feature) — they don't go through the subprocess \
                     GUI loader."
                .into(),
        }),
        // Catch-all only exists when at least one of vst3/clap/(au+macOS) is
        // disabled — otherwise all four PluginFormat variants are matched
        // explicitly above and rustc warns this arm is unreachable.
        #[cfg(not(all(
            feature = "vst3",
            feature = "clap",
            feature = "au",
            target_os = "macos"
        )))]
        _ => Err(BridgeError::LoadFailed {
            path: path.to_path_buf(),
            stage: LoadStage::Opening,
            reason: format!("No in-process GUI support for format {format:?}"),
        }),
    }
}

#[cfg(not(all(any(feature = "vst3", feature = "clap", feature = "au"), unix)))]
pub(crate) fn load_gui_instance(path: &Path) -> Result<Box<dyn PluginEditor>> {
    Err(BridgeError::LoadFailed {
        path: path.to_path_buf(),
        stage: LoadStage::Opening,
        reason: "In-process GUI support not built (no gui features enabled)".into(),
    })
}
