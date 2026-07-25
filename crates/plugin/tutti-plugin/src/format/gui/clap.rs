//! In-process CLAP GUI instance (editor only, no audio processing).
//!
//! Implements the host-side [`PluginEditor`](super::PluginEditor) trait. Though
//! `ClapGuiInstance` wraps the same `ClapLoaded` shape the server-side loader
//! activates, it is a *separate* host-process dlopen from the audio object in
//! the plugin-server subprocess — the two editor surfaces (`PluginEditor` here,
//! `PluginEditorHost` there) stay distinct by design, honestly modelling the
//! two-world split rather than collapsing it.

use super::PluginEditor;
use crate::error::{BridgeError, LoadStage, Result};
use crate::util::window::{EditorCapabilities, EditorSize, WindowHandle};
use std::path::Path;

pub(crate) struct ClapGuiInstance {
    inner: tutti_clap_host::ClapLoaded,
}

impl ClapGuiInstance {
    pub fn load(path: &Path) -> Result<Self> {
        // Resolve bundle directory to the actual binary.
        let resolved = crate::host::subprocess::resolve_bundle(path)?;
        // Editor-only load: gui/params/state work without activation, and this
        // instance must never be activated or process audio (audio runs in the
        // subprocess instance). `load_editor_only` encodes that contract.
        let inner =
            tutti_clap_host::ClapLoaded::load_editor_only(&resolved, None).map_err(|e| {
                BridgeError::LoadFailed {
                    path: path.to_path_buf(),
                    stage: LoadStage::Opening,
                    reason: format!("CLAP GUI-only load failed: {e}"),
                }
            })?;
        Ok(Self { inner })
    }
}

impl PluginEditor for ClapGuiInstance {
    fn open_editor(&mut self, parent: WindowHandle) -> Result<EditorSize> {
        let clap_handle = unsafe { tutti_clap_host::WindowHandle::from_raw(parent.as_ptr()) };
        let size = self
            .inner
            .open_editor(clap_handle)
            .map_err(|e| BridgeError::ProtocolError(format!("CLAP open_editor failed: {e}")))?;
        Ok(EditorSize {
            width: size.width,
            height: size.height,
        })
    }

    fn close_editor(&mut self) {
        self.inner.close_editor();
    }

    fn editor_idle(&mut self) {
        // If the plugin requested a param flush, perform it.
        if self.inner.poll_params_flush_requested() {
            let _ = self.inner.flush_params(vec![]);
        }
    }

    fn set_parameter(&mut self, id: u32, value: f64) {
        self.inner.set_parameter(id, value);
    }

    fn set_state(&mut self, data: &[u8]) -> Result<()> {
        self.inner
            .set_state(data)
            .map_err(|e| BridgeError::ProtocolError(format!("CLAP set_state failed: {e}")))
    }

    fn poll_gui_param_changes(&mut self) -> Vec<(u32, f32)> {
        // CLAP parameter changes from the GUI go through flush_params output events.
        // TODO: Capture output events from flush_params to forward to audio bridge.
        Vec::new()
    }

    fn editor_capabilities(&mut self) -> EditorCapabilities {
        // Pass-through from the host crate — the shared `EditorCapabilities`
        // is already the union of vst3/clap fields, and `appkit_autoresize_friendly`
        // defaults to `false` which matches CLAP's pinned-NSView behavior.
        self.inner.editor_capabilities()
    }

    fn set_editor_size(&mut self, requested: EditorSize) -> Result<EditorSize> {
        let snapped = self
            .inner
            .resize_editor(tutti_clap_host::EditorSize {
                width: requested.width,
                height: requested.height,
            })
            .map_err(|e| BridgeError::ProtocolError(format!("CLAP resize_editor failed: {e}")))?;
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
