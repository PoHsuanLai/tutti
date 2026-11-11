//! The editor-only AU instance an out-of-process host loads in its own process
//! ([`OutOfProcess::Editor`](tutti_plugin_types::OutOfProcess)).
//!
//! It is a second load of the same unit, separate from the audio instance in
//! the plugin server: the host mirrors parameters and state into it. It is an
//! [`AuLoaded`] that is never initialized (the GUI works without audio setup).

use std::path::Path;

use tutti_plugin_types::{
    ContentScale, Control, Editor, EditorCapabilities, EditorError, EditorSize, EditorWindow,
    InProcess, Load, MainThread, Normalized, ParamId, PluginError, PluginResult, WindowHandle,
};

use crate::instance::AuLoaded;

fn editor_error(reason: String) -> PluginError {
    PluginError::Editor(EditorError::PluginError(reason))
}

/// An Audio Unit loaded for its editor alone: never initialized, it shows the
/// Cocoa view and holds the state mirrored into it.
///
/// It is what [`OutOfProcess::load_editor`](tutti_plugin_types::OutOfProcess)
/// builds: `AuLoaded` is hostable in this process and over IPC:
///
/// ```
/// use tutti_plugin_types::{InProcess, OutOfProcess};
/// fn hostable<F: InProcess + OutOfProcess>() {}
/// hostable::<tutti_au_host::AuLoaded>();
/// ```
#[derive(Debug)]
pub struct AuEditorInstance {
    inner: AuLoaded,
}

impl AuEditorInstance {
    /// Loads the editor-only instance, resolved by [`AuLoaded::load`] so it
    /// finds the same component the audio path does.
    pub(crate) fn load(main: &MainThread, path: &Path, load: &Load) -> PluginResult<Self> {
        Ok(Self {
            inner: AuLoaded::load_in_process(main, path, load)?,
        })
    }

    /// The loaded unit behind the editor.
    #[must_use]
    pub fn loaded(&self) -> &AuLoaded {
        &self.inner
    }

    /// [`loaded`](Self::loaded), mutably.
    pub fn loaded_mut(&mut self) -> &mut AuLoaded {
        &mut self.inner
    }
}

impl Editor for AuEditorInstance {
    /// The unit's own [`Control::set_parameter`]: `AudioUnitSetParameter` takes
    /// **plain** units, and the host denormalizes against the one range table
    /// the audio path reads too, so the editor's knob and what is heard agree.
    fn set_parameter(&mut self, main: &MainThread, id: ParamId, value: Normalized) {
        Control::set_parameter(&mut self.inner, main, id, value);
    }

    fn set_state(&mut self, _main: &MainThread, data: &[u8]) -> PluginResult<()> {
        self.inner
            .set_class_info(data)
            .map_err(|e| editor_error(format!("AU set_state failed: {e}")))
    }

    fn save_state(&mut self, _main: &MainThread) -> PluginResult<Vec<u8>> {
        self.inner
            .class_info()
            .map_err(|e| editor_error(format!("AU get_state failed: {e}")))
    }

    fn poll_edits(&mut self, _main: &MainThread) -> Vec<(ParamId, f32)> {
        // AUv2 has no parameter-change notification from the GUI: a Cocoa view
        // calls `AudioUnitSetParameter` directly.
        Vec::new()
    }
}

impl EditorWindow for AuEditorInstance {
    fn has_editor(&self, main: &MainThread) -> bool {
        // Asks the live (uninitialized) unit for its Cocoa view property.
        EditorWindow::has_editor(&self.inner, main)
    }

    /// An AU view is an `NSView`, which lays out in logical pixels and reads its
    /// backing scale itself.
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
    fn editor_capabilities(&mut self, _main: &MainThread) -> EditorCapabilities {
        EditorCapabilities::default()
    }

    fn set_editor_size(
        &mut self,
        _main: &MainThread,
        _requested: EditorSize,
    ) -> PluginResult<EditorSize> {
        Err(editor_error("set_editor_size not supported".into()))
    }

    /// Nothing asked: this surface never queues a resize request.
    fn poll_editor_resize_request(&mut self, _main: &MainThread) -> Option<EditorSize> {
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
        let Ok(mut gui) = AuEditorInstance::load(
            &main,
            Path::new("/System/Library/Components/CoreAudio.component"),
            &Load::new().plugin("au.aufx.dely.appl"),
        ) else {
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
