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
use crate::protocol::{Normalized, ParamAddress, ParamRange};

/// Look up declared bounds for `id` in a table sorted by id.
fn lookup_range(table: &[(u32, (f64, f64))], id: u32) -> Option<(f64, f64)> {
    table
        .binary_search_by_key(&id, |(k, _)| *k)
        .ok()
        .map(|i| table[i].1)
}
use crate::util::window::{EditorCapabilities, EditorSize, WindowHandle};
use std::path::Path;

pub(crate) struct ClapGuiInstance {
    inner: tutti_clap_host::ClapLoaded,
    /// Declared plain-unit bounds per parameter id, sorted, captured at load.
    ///
    /// CLAP parameter values are in the plugin's own units where this host
    /// speaks normalized, so a mirrored write has to be denormalized. Cached
    /// because `ClapLoaded::parameter_range` is a linear scan that makes one
    /// FFI call per parameter to find one id — acceptable once at load, not on
    /// every knob movement.
    param_ranges: Vec<(u32, (f64, f64))>,
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
        // One pass over the catalog instead of a scan per write. Params are
        // readable on an unactivated instance, which is what this GUI-only
        // load is.
        let mut param_ranges: Vec<(u32, (f64, f64))> = inner
            .parameter_list()
            .into_iter()
            .filter_map(|p| {
                let id = p.id.opaque()?.get();
                p.bounds().map(|b| (id, b))
            })
            .collect();
        param_ranges.sort_unstable_by_key(|(id, _)| *id);

        Ok(Self {
            inner,
            param_ranges,
        })
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

    fn open_floating_editor(&mut self) -> Result<()> {
        // No transient parent: this crate does not hold the host's window, and
        // `set_transient` is a stacking hint the plugin may ignore anyway. The
        // title is what the host would have put on a window it owned.
        let title = std::ffi::CString::new(self.inner.info().name.as_str())
            .unwrap_or_else(|_| c"Plugin Editor".to_owned());
        self.inner
            .open_floating_editor(None, &title)
            .map_err(|e| BridgeError::ProtocolError(format!("CLAP floating open failed: {e}")))
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

    fn set_parameter(&mut self, id: ParamAddress, value: Normalized) {
        // `clap_id` is opaque; a VST2 index addresses nothing here.
        let Some(id) = id.opaque() else { return };

        // CLAP values are in the plugin's native plain range, so the host's
        // normalized value is denormalized against the declared bounds — the
        // same conversion the subprocess loader applies on the audio path.
        // Without it, a mirrored write moved the editor to the wrong position
        // while the audio path went where it was asked.
        //
        // A parameter that declared no bounds is written through unconverted,
        // matching the loader: `ParamRange::Normalized` means there is nothing
        // to convert against, and inventing `0..=1` would rescale a parameter
        // the plugin never described.
        let plain = match lookup_range(&self.param_ranges, id.get()) {
            Some((min, max)) => ParamRange::Plain {
                min,
                max,
                default: min,
            }
            .to_plain(value.get()),
            None => value.get(),
        };
        self.inner.set_parameter(id.get(), plain);
    }

    fn set_state(&mut self, data: &[u8]) -> Result<()> {
        self.inner
            .set_state(data)
            .map_err(|e| BridgeError::ProtocolError(format!("CLAP set_state failed: {e}")))
    }

    fn poll_gui_param_changes(&mut self) -> Vec<(ParamAddress, f32)> {
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
