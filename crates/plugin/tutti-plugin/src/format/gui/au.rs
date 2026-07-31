//! In-process Audio Unit GUI instance (editor only, no audio processing).
//! macOS only.
//!
//! Implements the host-side [`PluginEditor`](super::PluginEditor) trait. Even
//! though `AuInstance` (in `loaders/au.rs`) owns the *subprocess-side* editor
//! open/close/state paths, this host-process object is a separate dlopen: the
//! two editor surfaces stay distinct by design, one per world.

use super::PluginEditor;
use crate::error::{BridgeError, LoadStage, Result};
use crate::protocol::ParamAddress;
use crate::util::window::{EditorSize, WindowHandle};
use std::path::Path;

pub(crate) struct AuGuiInstance {
    inner: tutti_au_host::AuInstance,
    editor: Option<tutti_au_host::AuEditor>,
}

impl AuGuiInstance {
    pub fn load(path: &Path) -> Result<Self> {
        // AU plugins are system-registered; find matching component by bundle name.
        let bundle_name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        let components = tutti_au_host::component::enumerate_components();
        let matching = components.iter().find(|c| {
            c.name.contains(&bundle_name)
                || c.name.ends_with(&bundle_name)
                || c.name.split(": ").last().is_some_and(|n| n == bundle_name)
        });

        let component_handle =
            matching
                .map(|info| info.component)
                .ok_or_else(|| BridgeError::LoadFailed {
                    path: path.to_path_buf(),
                    stage: LoadStage::Scanning,
                    reason: format!("No Audio Unit component found matching '{bundle_name}'"),
                })?;

        // Create instance but do NOT initialize — GUI works without audio setup.
        let inner = unsafe { tutti_au_host::AuInstance::new(component_handle, 44100.0, 512) }
            .map_err(|e| BridgeError::LoadFailed {
                path: path.to_path_buf(),
                stage: LoadStage::Instantiation,
                reason: format!("AU GUI-only load failed: {e}"),
            })?;

        Ok(Self {
            inner,
            editor: None,
        })
    }
}

impl PluginEditor for AuGuiInstance {
    fn open_editor(&mut self, parent: WindowHandle) -> Result<EditorSize> {
        let parent_handle = unsafe { tutti_au_host::WindowHandle::from_raw(parent.as_ptr()) };
        let editor =
            unsafe { tutti_au_host::AuEditor::open(self.inner.raw_unit(), Some(parent_handle)) }
                .map_err(|e| BridgeError::ProtocolError(format!("AU open_editor failed: {e}")))?;
        let size = editor.editor_size();
        self.editor = Some(editor);
        Ok(EditorSize {
            width: size.width,
            height: size.height,
        })
    }

    fn close_editor(&mut self) {
        if let Some(mut ed) = self.editor.take() {
            ed.close();
        }
    }

    fn editor_idle(&mut self) {
        // AUv2 Cocoa views are driven by the AppKit run loop; no explicit idle needed.
    }

    fn set_parameter(&mut self, id: ParamAddress, value: f64) {
        // `AudioUnitParameterID` is opaque; a VST2 index addresses nothing here.
        let Some(id) = id.opaque() else { return };
        let _ = self.inner.set_parameter(id.get(), value as f32);
    }

    fn set_state(&mut self, data: &[u8]) -> Result<()> {
        self.inner
            .load_state(data)
            .map_err(|e| BridgeError::ProtocolError(format!("AU set_state failed: {e}")))
    }

    fn poll_gui_param_changes(&mut self) -> Vec<(ParamAddress, f32)> {
        // AUv2 doesn't have a built-in parameter change notification from GUI.
        // Parameter changes from Cocoa views go through AudioUnitSetParameter directly.
        Vec::new()
    }
}
