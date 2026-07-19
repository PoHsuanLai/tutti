//! Editor window lifecycle.
//!
//! `vst::editor::Editor::open` wants a raw platform window pointer; we
//! unpack [`WindowHandle`]'s `*mut c_void` at this single site so the rest
//! of the crate never touches the unsafe conversion.

use crate::error::{Result, Vst2Error};
use crate::instance::Vst2Instance;
use crate::types::{EditorSize, WindowHandle};

impl Vst2Instance {
    /// Embed the plugin's editor into `parent`.
    ///
    /// Returns the editor's reported size on success.
    ///
    /// On macOS this MUST be called from the AppKit main thread —
    /// platform GUI toolkits (Cocoa, AppKit) require it. Calling from a
    /// worker thread will crash deep inside the plugin's UI code.
    pub fn open_editor(&mut self, parent: WindowHandle) -> Result<EditorSize> {
        let editor = &mut self
            .handle
            .editor
            .as_mut()
            .ok_or_else(|| Vst2Error::EditorError("Plugin has no editor".into()))?
            .0;

        let opened = editor.open(parent.as_ptr());
        if !opened {
            return Err(Vst2Error::EditorError(
                "VST2 editor.open() returned false".into(),
            ));
        }
        let size = editor.size();
        Ok(EditorSize {
            width: size.0 as u32,
            height: size.1 as u32,
        })
    }

    /// Tear down the editor view. Safe to call when no editor is open.
    pub fn close_editor(&mut self) {
        if let Some(editor) = self.handle.editor.as_mut() {
            editor.0.close();
        }
    }

    /// Drive the editor's idle hook. Should be called periodically (~30Hz)
    /// from the host's main thread while the editor is open. JUCE-based
    /// editors rely on this to repaint and process input events.
    pub fn editor_idle(&mut self) {
        if let Some(editor) = self.handle.editor.as_mut() {
            editor.0.idle();
        }
    }
}
