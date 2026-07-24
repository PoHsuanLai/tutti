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
        tutti_plugin_types::assert_main_thread();

        let editor = &mut self
            .handle
            .editor
            .as_mut()
            .ok_or_else(|| Vst2Error::EditorError("Plugin has no editor".into()))?
            .0;

        // SDK convention: query the editor's size (effEditGetRect) BEFORE
        // embedding it (effEditOpen), so the host sizes its window before the
        // plugin attaches. vst-rs 0.3.0's `Editor` trait exposes no dedicated
        // `get_rect()` — `size()` is the only size query — so we read it once
        // before `open()`. If the plugin reports a degenerate pre-open size
        // (0×0, common when a plugin only computes its rect on open), we fall
        // back to the post-open `size()`.
        let pre = editor.size();

        let opened = editor.open(parent.as_ptr());
        if !opened {
            return Err(Vst2Error::EditorError(
                "VST2 editor.open() returned false".into(),
            ));
        }

        let (w, h) = if pre.0 > 0 && pre.1 > 0 {
            pre
        } else {
            editor.size()
        };
        Ok(EditorSize {
            width: w as u32,
            height: h as u32,
        })
    }

    /// Tear down the editor view. Safe to call when no editor is open.
    pub fn close_editor(&mut self) {
        tutti_plugin_types::assert_main_thread();

        if let Some(editor) = self.handle.editor.as_mut() {
            editor.0.close();
        }
    }

    /// Drive the editor's idle hook. Should be called periodically (~30Hz)
    /// from the host's main thread while the editor is open. JUCE-based
    /// editors rely on this to repaint and process input events.
    pub fn editor_idle(&mut self) {
        tutti_plugin_types::assert_main_thread();

        if let Some(editor) = self.handle.editor.as_mut() {
            editor.0.idle();
        }
    }
}
