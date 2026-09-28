//! Editor window lifecycle.
//!
//! `vst::editor::Editor::open` wants a raw platform window pointer.
//! [`WindowHandle`]'s `*mut c_void` is unpacked at this single site, so the rest
//! of the crate never touches the unsafe conversion.

use crate::error::{Result, Vst2Error};
use crate::instance::Vst2Instance;
use crate::types::{EditorSize, WindowHandle};

impl Vst2Instance {
    /// Returns `true` if the plugin has a native editor.
    ///
    /// Probed once at load, so this does not call into the plugin. `false`
    /// means [`open_editor`](Self::open_editor) has nothing to open. The same
    /// answer as [`PluginInfo::has_editor`](crate::PluginInfo::has_editor).
    pub fn has_editor(&self) -> bool {
        self.handle.has_editor()
    }

    /// Opens the plugin's editor embedded in `parent` and returns its size in
    /// pixels.
    ///
    /// `parent` is the native view or window to embed into (`NSView*` on
    /// macOS, `HWND` on Windows, an X11 window id on Linux). While the editor is
    /// open, call [`editor_idle`](Self::editor_idle) regularly, and
    /// [`close_editor`](Self::close_editor) before destroying `parent`.
    ///
    /// # Errors
    ///
    /// Returns [`Vst2Error::EditorError`] if the plugin has no editor or
    /// refuses to open it.
    ///
    /// # Panics
    ///
    /// Main thread only; platform GUI toolkits require it, and on macOS a call
    /// from another thread crashes inside the plugin's UI code. In debug builds,
    /// panics if called off the thread registered with
    /// [`tutti_plugin_types::mark_main_thread`].
    pub fn open_editor(&mut self, parent: WindowHandle) -> Result<EditorSize> {
        tutti_plugin_types::assert_main_thread();

        let editor = &mut self
            .handle
            .editor
            .as_mut()
            .ok_or_else(|| Vst2Error::EditorError("Plugin has no editor".into()))?
            .0;

        // The rect's *origin* is deliberately not read. `effEditGetRect`
        // reports `left`/`top` as well as the extent, but this host embeds the
        // view into a `parent` window it owns, so the parent decides placement
        // and a plugin-requested screen position has nothing to act on. It
        // would matter for a floating editor, which this path does not offer.
        //
        // SDK convention: query the editor's size (effEditGetRect) BEFORE
        // embedding it (effEditOpen), so the host sizes its window before the
        // plugin attaches. vst-rs 0.3.0's `Editor` trait exposes no dedicated
        // `get_rect()` — `size()` is the only size query — so it is read once
        // before `open()`. A degenerate pre-open size (0x0, common when a plugin
        // only computes its rect on open) falls back to the post-open `size()`.
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

    /// Closes the editor view. Does nothing when no editor is open.
    ///
    /// # Panics
    ///
    /// Main thread only; in debug builds, panics if called off the thread
    /// registered with [`tutti_plugin_types::mark_main_thread`].
    pub fn close_editor(&mut self) {
        tutti_plugin_types::assert_main_thread();

        if let Some(editor) = self.handle.editor.as_mut() {
            editor.0.close();
        }
    }

    /// Drives the editor's idle hook.
    ///
    /// Call periodically (around 30 Hz) from the main thread while the editor
    /// is open; JUCE-based editors rely on it to repaint and process input.
    ///
    /// # Panics
    ///
    /// Main thread only; in debug builds, panics if called off the thread
    /// registered with [`tutti_plugin_types::mark_main_thread`].
    pub fn editor_idle(&mut self) {
        tutti_plugin_types::assert_main_thread();

        if let Some(editor) = self.handle.editor.as_mut() {
            editor.0.idle();
        }
    }
}
