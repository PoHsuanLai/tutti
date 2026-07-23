//! In-process VST3 GUI instance (editor only, no audio processing).

use super::GuiInstance;
use crate::error::{BridgeError, LoadStage, Result};
use crate::util::window::{EditorCapabilities, EditorSize, WindowHandle};
use std::path::Path;

pub(crate) struct Vst3GuiInstance {
    inner: tutti_vst3_host::Vst3Loaded,
}

impl Vst3GuiInstance {
    pub fn load(path: &Path) -> Result<Self> {
        let resolved = crate::host::subprocess::resolve_bundle(path)?;
        let inner =
            tutti_vst3_host::Vst3Loaded::load(&resolved).map_err(|e| BridgeError::LoadFailed {
                path: path.to_path_buf(),
                stage: LoadStage::Opening,
                reason: format!("VST3 GUI-only load failed: {e}"),
            })?;
        Ok(Self { inner })
    }
}

impl GuiInstance for Vst3GuiInstance {
    fn open_editor(&mut self, parent: WindowHandle) -> Result<EditorSize> {
        tracing::info!(
            "[vst3-gui] open_editor: creating vst3 handle from ptr {:?}",
            parent.as_ptr()
        );
        let vst3_handle = unsafe { tutti_vst3_host::WindowHandle::from_raw(parent.as_ptr()) };
        tracing::info!("[vst3-gui] open_editor: calling inner.open_editor");
        let size = self
            .inner
            .open_editor(vst3_handle)
            .map_err(|e| BridgeError::ProtocolError(format!("VST3 open_editor failed: {e}")))?;
        tracing::info!(
            "[vst3-gui] open_editor: success {}x{}",
            size.width,
            size.height
        );
        Ok(EditorSize {
            width: size.width,
            height: size.height,
        })
    }

    fn close_editor(&mut self) {
        self.inner.close_editor();
    }

    fn editor_idle(&mut self) {
        // VST3 editors don't have explicit idle.
    }

    fn set_parameter(&mut self, id: u32, value: f64) {
        self.inner.set_parameter(id, value);
    }

    fn set_state(&mut self, data: &[u8]) -> Result<()> {
        self.inner
            .set_state(data)
            .map_err(|e| BridgeError::ProtocolError(format!("VST3 set_state failed: {e}")))?;
        Ok(())
    }

    fn poll_gui_param_changes(&mut self) -> Vec<(u32, f32)> {
        // Drain through the unified notification poll so `restartComponent`
        // requests (latency / IO / param re-reads) are applied to host state
        // instead of being silently dropped. The GUI path forwards param
        // edits up; the `RestartOutcome` side effects (latency re-read, bus
        // re-enumeration) are applied in-place by `poll_plugin_notifications`.
        // Progress / unit notifications are not surfaced through the GUI param
        // path.
        let notifications = self.inner.poll_plugin_notifications();
        notifications
            .param_edits
            .into_iter()
            .filter_map(|e| {
                use tutti_vst3_host::ParameterEditEvent;
                match e {
                    ParameterEditEvent::PerformEdit { param_id, value } => {
                        Some((param_id, value as f32))
                    }
                    _ => None,
                }
            })
            .collect()
    }

    fn editor_capabilities(&mut self) -> EditorCapabilities {
        let inner = self.inner.editor_capabilities();
        EditorCapabilities {
            resize: inner.resize,
            appkit_autoresize_friendly: true,
            ..EditorCapabilities::default()
        }
    }

    fn set_editor_size(&mut self, requested: EditorSize) -> Result<EditorSize> {
        let snapped = self
            .inner
            .resize_editor(tutti_vst3_host::EditorSize {
                width: requested.width,
                height: requested.height,
            })
            .map_err(|e| BridgeError::ProtocolError(format!("VST3 resize_editor failed: {e}")))?;
        Ok(EditorSize {
            width: snapped.width,
            height: snapped.height,
        })
    }

    fn poll_editor_resize_request(&mut self) -> Option<EditorSize> {
        self.inner
            .poll_editor_resize_request()
            .map(|sz| EditorSize {
                width: sz.width,
                height: sz.height,
            })
    }
}
