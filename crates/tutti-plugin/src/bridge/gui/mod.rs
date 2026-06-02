//! In-process plugin editor.
//!
//! The plugin-server subprocess owns audio processing; the editor must
//! live in the host process because platform GUI toolkits are
//! host-process-local. Each supported format (VST3, CLAP, AU) has its
//! own [`GuiInstance`] implementation that dlopens the plugin library
//! a second time purely for the editor.

#[cfg(all(feature = "au", target_os = "macos"))]
mod au;
#[cfg(all(feature = "clap", unix))]
mod clap;
#[cfg(all(feature = "vst3", unix))]
mod vst3;

use crate::error::{BridgeError, LoadStage, Result};
use crate::window::{EditorCapabilities, EditorSize, WindowHandle};
use std::path::Path;

/// In-process plugin instance used only for GUI and parameter display.
pub(crate) trait GuiInstance: Send {
    fn open_editor(&mut self, parent: WindowHandle) -> Result<EditorSize>;
    fn close_editor(&mut self);
    fn editor_idle(&mut self);
    fn set_parameter(&mut self, id: u32, value: f64);
    fn set_state(&mut self, data: &[u8]) -> Result<()>;
    /// Poll GUI-originated parameter changes to forward to the audio bridge.
    fn poll_gui_param_changes(&mut self) -> Vec<(u32, f32)>;

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
pub(crate) fn load_gui_instance(path: &Path) -> Result<Box<dyn GuiInstance>> {
    use crate::discovery::{format_from_path, PluginFormat};

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
                     `vst2-in-process` feature) — they don't go through the \
                     subprocess GUI loader."
                .into(),
        }),
        PluginFormat::Wasm => Err(BridgeError::LoadFailed {
            path: path.to_path_buf(),
            stage: LoadStage::Opening,
            reason: "WASM audio plugins have no editor in v0.1 of \
                     `dawai:audio-plugin`. Ship a panel UI from the \
                     editor extension side instead."
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
pub(crate) fn load_gui_instance(path: &Path) -> Result<Box<dyn GuiInstance>> {
    Err(BridgeError::LoadFailed {
        path: path.to_path_buf(),
        stage: LoadStage::Opening,
        reason: "In-process GUI support not built (no gui features enabled)".into(),
    })
}
