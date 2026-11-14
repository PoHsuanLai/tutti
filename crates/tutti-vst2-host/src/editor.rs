//! Editor window lifecycle.
//!
//! The editor is the instance's own (`PluginInstance::open_editor` and its siblings, each
//! taking a `&MainThread`): VST2 fuses the editor and the audio processor into one `AEffect`,
//! so there is no second object to hold.

use crate::error::{Result, Vst2Error};
use crate::loaded::Vst2Loaded;
use crate::types::WindowHandle;
use dpi::Size;
use tutti_plugin_types::{EditorUnit, MainThread};

/// `width` by `height` as an `ERect` extent counts them: device pixels on Windows and
/// X11, points on macOS, which is how the window system counts its own windows. The one
/// place a VST2 size gets its unit.
pub(crate) fn rect_size(width: u32, height: u32) -> Size {
    EditorUnit::WINDOWING.size(width, height)
}

impl Vst2Loaded {
    /// Opens the plugin's editor embedded in `parent` and returns its size in
    /// the unit of an `ERect`: pixels, or points on macOS.
    ///
    /// `parent` is the native view or window to embed into (`NSView*` on
    /// macOS, `HWND` on Windows, an X11 window id on Linux). While the editor is
    /// open, call [`EditorWindow::editor_idle`](tutti_plugin_types::EditorWindow::editor_idle)
    /// regularly, and [`EditorWindow::close_editor`](tutti_plugin_types::EditorWindow::close_editor)
    /// before destroying `parent`. The body of
    /// [`EditorWindow::open_editor`](tutti_plugin_types::EditorWindow::open_editor).
    ///
    /// # Errors
    ///
    /// Returns [`Vst2Error::EditorError`] if the plugin has no editor or
    /// refuses to open it.
    ///
    /// # Panics
    ///
    /// Main thread only; platform GUI toolkits require it, and on macOS a call
    /// from another thread crashes inside the plugin's UI code, which the
    /// `&MainThread` proves the caller is not on.
    pub(crate) fn open_editor_in(
        &mut self,
        main: &MainThread,
        parent: WindowHandle,
    ) -> Result<Size> {
        if !self.instance.has_editor() {
            return Err(Vst2Error::EditorError("Plugin has no editor".into()));
        }

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
        let pre = self.instance.editor_size(main);

        let opened = self.instance.open_editor(main, parent);
        if !opened {
            return Err(Vst2Error::EditorError(
                "VST2 editor.open() returned false".into(),
            ));
        }

        let (w, h) = if pre.0 > 0 && pre.1 > 0 {
            pre
        } else {
            self.instance.editor_size(main)
        };
        Ok(rect_size(w as u32, h as u32))
    }
}
