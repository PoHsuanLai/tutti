//! The editor-only VST3 instance an out-of-process host loads in its own
//! process ([`OutOfProcess::Editor`](tutti_plugin_types::OutOfProcess)).
//!
//! It is a second load of the same plugin, separate from the audio instance in
//! the plugin server: the host mirrors parameters and state into it and
//! collects the edits the user makes in it.

use std::path::Path;

use tutti_plugin_types::{
    AutomationMode, ContentScale, Control, Editor, EditorCapabilities, EditorError, EditorSize,
    EditorWindow, InProcess, Load, MainThread, Normalized, ParamId, PluginError, PluginResult,
    WindowHandle,
};

use super::loaded::Vst3Loaded;

fn editor_error(reason: String) -> PluginError {
    PluginError::Editor(EditorError::PluginError(reason))
}

/// A VST3 plugin loaded for its editor alone: never activated, it shows the
/// GUI and reports the edits made in it.
///
/// It is what [`OutOfProcess::load_editor`](tutti_plugin_types::OutOfProcess)
/// builds: `Vst3Loaded` is hostable in this process and over IPC (the twin of
/// the refusal on `Vst2Loaded`, design 021 §3.4):
///
/// ```
/// use tutti_plugin_types::{InProcess, OutOfProcess};
/// fn hostable<F: InProcess + OutOfProcess>() {}
/// hostable::<tutti_vst3_host::Vst3Loaded>();
/// ```
#[derive(Debug)]
pub struct Vst3Editor {
    inner: Vst3Loaded,
    /// `PerformEdit`s drained but not yet polled: one notification drain
    /// serves both [`Editor::poll_edits`] and [`Editor::take_dirty`].
    edits: Vec<(ParamId, f32)>,
    /// The plugin called `setDirty(true)` since the last
    /// [`take_dirty`](Editor::take_dirty).
    dirty: bool,
}

impl Vst3Editor {
    /// Loads the editor-only instance. It is never activated and never
    /// processes audio: the audio instance runs elsewhere.
    pub(crate) fn load(main: &MainThread, path: &Path, load: &Load) -> PluginResult<Self> {
        Ok(Self {
            inner: Vst3Loaded::load_in_process(main, path, load)?,
            edits: Vec::new(),
            dirty: false,
        })
    }

    /// Drains the editor instance's notifications into `edits` and `dirty`.
    ///
    /// NOT HANDLED: the returned `RestartOutcome`. In particular a
    /// `kParamValuesChanged` (or `notifications_overflowed`) from this editor
    /// instance is dropped, so a preset loaded inside the plugin's own UI does
    /// not refresh the host's parameter values (its state still reaches the
    /// audio instance on close, design 021 Q8). Progress / unit notifications
    /// are not surfaced either.
    fn drain(&mut self, main: &MainThread) {
        use crate::{ParameterEditEvent, PluginNotification};
        let (edits, dirty) = (&mut self.edits, &mut self.dirty);
        let _restart = self.inner.poll_plugin_notifications(main, |n| match n {
            // The plugin's editor reported one of its own `ParamID`s.
            PluginNotification::Edit(ParameterEditEvent::PerformEdit { param_id, value }) => {
                edits.push((param_id.into(), value as f32));
            }
            PluginNotification::Edit(ParameterEditEvent::SetDirty(true)) => *dirty = true,
            _ => {}
        });
    }

    /// The loaded plugin behind the editor.
    #[must_use]
    pub fn loaded(&self) -> &Vst3Loaded {
        &self.inner
    }

    /// [`loaded`](Self::loaded), mutably.
    pub fn loaded_mut(&mut self) -> &mut Vst3Loaded {
        &mut self.inner
    }
}

impl Editor for Vst3Editor {
    fn set_parameter(&mut self, main: &MainThread, id: ParamId, value: Normalized) {
        // VST3's `setParamNormalized` takes the host domain unchanged.
        Control::set_parameter(&mut self.inner, main, id, value);
    }

    fn set_automation_state(&mut self, main: &MainThread, mode: AutomationMode) {
        // Deliver the host automation-state advisory to the GUI instance: this
        // is the instance whose editor shows the knob-glow feedback. The host
        // crate encodes the format-neutral mode onto the VST3 `IAutomationState`
        // bitmask.
        Control::set_automation_state(&mut self.inner, main, mode);
    }

    fn set_state(&mut self, main: &MainThread, data: &[u8]) -> PluginResult<()> {
        Control::load_state(&mut self.inner, main, data)
            .map_err(|e| editor_error(format!("VST3 set_state failed: {e}")))?;
        Ok(())
    }

    fn poll_edits(&mut self, main: &MainThread) -> Vec<(ParamId, f32)> {
        self.drain(main);
        std::mem::take(&mut self.edits)
    }

    fn save_state(&mut self, main: &MainThread) -> PluginResult<Vec<u8>> {
        Control::save_state(&mut self.inner, main)
    }

    fn take_dirty(&mut self, main: &MainThread) -> bool {
        self.drain(main);
        std::mem::take(&mut self.dirty)
    }

    fn editor_capabilities(&mut self, main: &MainThread) -> EditorCapabilities {
        let inner = self.inner.editor_capabilities(main);
        EditorCapabilities {
            resize: inner.resize,
            appkit_autoresize_friendly: true,
            ..EditorCapabilities::default()
        }
    }

    fn set_editor_size(
        &mut self,
        main: &MainThread,
        requested: EditorSize,
    ) -> PluginResult<EditorSize> {
        let snapped = self
            .inner
            .resize_editor(
                main,
                EditorSize {
                    width: requested.width,
                    height: requested.height,
                },
            )
            .map_err(|e| editor_error(format!("VST3 resize_editor failed: {e}")))?;
        Ok(EditorSize {
            width: snapped.width,
            height: snapped.height,
        })
    }

    fn poll_editor_resize_request(&mut self, main: &MainThread) -> Option<EditorSize> {
        self.inner
            .poll_editor_resize_request(main)
            .map(|sz| EditorSize {
                width: sz.width,
                height: sz.height,
            })
    }
}

impl EditorWindow for Vst3Editor {
    fn has_editor(&self, main: &MainThread) -> bool {
        EditorWindow::has_editor(&self.inner, main)
    }

    fn open_editor(
        &mut self,
        main: &MainThread,
        parent: WindowHandle,
        scale: ContentScale,
    ) -> PluginResult<EditorSize> {
        let size = self
            .inner
            .embed_editor(main, parent, scale)
            .map_err(|e| editor_error(format!("VST3 open_editor failed: {e}")))?;
        Ok(EditorSize {
            width: size.width,
            height: size.height,
        })
    }

    fn close_editor(&mut self, main: &MainThread) {
        EditorWindow::close_editor(&mut self.inner, main);
    }

    fn editor_idle(&mut self, _main: &MainThread) {
        // VST3 editors don't have explicit idle.
    }
}
