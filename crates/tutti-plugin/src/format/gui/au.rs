//! In-process Audio Unit GUI instance (editor only, no audio processing).
//! macOS only.
//!
//! Implements the shared [`Editor`](tutti_plugin_types::Editor) trait. Even
//! though the server's AU loader (`loaders/au.rs`) owns the *subprocess-side* editor
//! open/close/state paths, this host-process object is a separate dlopen: the
//! two editor surfaces stay distinct by design, one per world.

use std::path::Path;

use tutti_plugin_types::{BridgeError, BridgeResult, Control, Load, MainThread};
use tutti_plugin_types::{ContentScale, EditorSize, WindowHandle};
use tutti_plugin_types::{Editor, EditorError, EditorWindow, PluginError, PluginResult};
use tutti_plugin_types::{Normalized, ParamId};

/// The editor-only unit: an [`AuLoaded`](tutti_au_host::AuLoaded) that is never
/// initialized (the GUI works without audio setup), loaded and resolved by the
/// host's own [`AuLoaded::load`](tutti_au_host::AuLoaded::load), so this mirror
/// finds the same component the audio path does.
pub(crate) struct AuGuiInstance {
    inner: tutti_au_host::AuLoaded,
}

impl AuGuiInstance {
    pub fn load(main: &MainThread, path: &Path) -> BridgeResult<Self> {
        let inner =
            tutti_au_host::AuLoaded::load(main, path, &Load::new()).map_err(|e| match e {
                PluginError::Load { stage, reason } => BridgeError::LoadFailed {
                    path: path.to_path_buf(),
                    stage,
                    reason,
                },
                other => BridgeError::LoadFailed {
                    path: path.to_path_buf(),
                    stage: tutti_plugin_types::LoadStage::Instantiation,
                    reason: other.to_string(),
                },
            })?;
        Ok(Self { inner })
    }
}

impl Editor for AuGuiInstance {
    /// The unit's own [`Control::set_parameter`]: `AudioUnitSetParameter` takes
    /// **plain** units, and the host denormalizes against the one range table
    /// the audio path reads too, so the editor's knob and what is heard agree.
    fn set_parameter(&mut self, main: &MainThread, id: ParamId, value: Normalized) {
        Control::set_parameter(&mut self.inner, main, id, value);
    }

    fn set_state(&mut self, _main: &MainThread, data: &[u8]) -> PluginResult<()> {
        self.inner.set_class_info(data).map_err(|e| {
            PluginError::Editor(EditorError::PluginError(format!(
                "AU set_state failed: {e}"
            )))
        })
    }

    fn save_state(&mut self, _main: &MainThread) -> PluginResult<Vec<u8>> {
        self.inner.class_info().map_err(|e| {
            PluginError::Editor(EditorError::PluginError(format!(
                "AU get_state failed: {e}"
            )))
        })
    }

    fn poll_edits(&mut self, _main: &MainThread) -> Vec<(ParamId, f32)> {
        // AUv2 doesn't have a built-in parameter change notification from GUI.
        // Parameter changes from Cocoa views go through AudioUnitSetParameter directly.
        Vec::new()
    }
}

impl EditorWindow for AuGuiInstance {
    fn has_editor(&self, main: &MainThread) -> bool {
        // Asks the live (uninitialized) unit for its Cocoa view property.
        EditorWindow::has_editor(&self.inner, main)
    }

    /// The unit's own `EditorWindow`: an AU view is an `NSView`, which lays out
    /// in logical pixels and reads its backing scale itself.
    fn open_editor(
        &mut self,
        main: &MainThread,
        parent: WindowHandle,
        scale: ContentScale,
    ) -> PluginResult<EditorSize> {
        EditorWindow::open_editor(&mut self.inner, main, parent, scale)
    }

    fn close_editor(&mut self, main: &MainThread) {
        EditorWindow::close_editor(&mut self.inner, main);
    }

    fn editor_idle(&mut self, _main: &MainThread) {
        // AUv2 Cocoa views are driven by the AppKit run loop; no explicit idle needed.
    }

    /// The default capabilities: this surface reports no resize support.
    fn editor_capabilities(
        &mut self,
        _main: &tutti_plugin_types::MainThread,
    ) -> tutti_plugin_types::EditorCapabilities {
        tutti_plugin_types::EditorCapabilities::default()
    }

    /// Not supported here.
    fn set_editor_size(
        &mut self,
        _main: &tutti_plugin_types::MainThread,
        _requested: tutti_plugin_types::EditorSize,
    ) -> tutti_plugin_types::PluginResult<tutti_plugin_types::EditorSize> {
        Err(tutti_plugin_types::PluginError::Editor(
            tutti_plugin_types::EditorError::PluginError("set_editor_size not supported".into()),
        ))
    }

    /// Nothing asked: this surface never queues a resize request.
    fn poll_editor_resize_request(
        &mut self,
        _main: &tutti_plugin_types::MainThread,
    ) -> Option<tutti_plugin_types::EditorSize> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A mirrored write is denormalized against the declared range.**
    /// `set_parameter_rt` hands the same normalized value to the audio path and
    /// to this mirror, but `AudioUnitSetParameter` takes plain units. Writing
    /// `1.0` to AUDelay's "Lowpass Cutoff" (`[10, 22050]` Hz) once moved the
    /// editor's knob to 1 Hz while the audio path went to full scale. The unit's
    /// own reading after the write is what this asserts.
    #[test]
    fn the_gui_mirror_denormalizes_against_the_declared_range() {
        let main = tutti_fixtures::main_thread();
        let Ok(mut gui) = AuGuiInstance::load(&main, Path::new("AUDelay")) else {
            eprintln!("AUDelay unavailable; skipping");
            return;
        };

        // A parameter whose range is not 0..=1, or normalized and plain coincide
        // and the test cannot tell a conversion from a pass-through. AUDelay's
        // "Delay Time" is `[0, 2]` s and "Lowpass Cutoff" `[10, 22050]` Hz
        // (measured, macOS 15.6).
        let wide = gui
            .inner
            .get_parameter_list()
            .into_iter()
            .find(|p| p.range.min != 0.0 || p.range.max != 1.0)
            .expect("AUDelay declares parameters with ranges wider than 0..=1");
        let id = ParamId::new(wide.id);

        Editor::set_parameter(&mut gui, &main, id, Normalized::of(1.0));
        assert_eq!(gui.inner.get_parameter(wide.id).unwrap(), wide.range.max);
        Editor::set_parameter(&mut gui, &main, id, Normalized::of(0.0));
        assert_eq!(gui.inner.get_parameter(wide.id).unwrap(), wide.range.min);
    }
}
