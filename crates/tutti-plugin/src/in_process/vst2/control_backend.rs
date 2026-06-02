//! `ControlBackend` impl for the in-process VST2 host.
//!
//! Holds the same `Arc<Mutex<Vst2Instance>>` the audio unit holds. GUI
//! thread calls take the lock for the duration of one plugin operation
//! — short for parameter / state methods, potentially long for editor
//! ones. The audio thread always uses `try_lock` (in
//! `super::audio_unit`) and falls back to silence on contention so a
//! slow `editor_idle` can't underrun audio.

use std::ffi::c_void;
use std::sync::Arc;

use parking_lot::Mutex;
use tutti_vst2_host::Vst2Instance;

use crate::audio_node::ParameterChangeSink;
use crate::control_backend::ControlBackend;
use crate::error::EditorError;
use crate::protocol::ParameterInfo;
use crate::window::EditorSize;

/// Bundles the shared Mutex with the parameter-change sink so editor
/// idle calls can drain plugin-internal automation events and notify
/// the user-installed callback.
pub(crate) struct InProcessVst2Backend {
    pub(crate) inner: Arc<Mutex<Vst2Instance>>,
    pub(crate) param_sink: ParameterChangeSink,
}

impl ControlBackend for InProcessVst2Backend {
    fn open_editor(&self, parent_ptr: *mut c_void) -> std::result::Result<EditorSize, EditorError> {
        // SAFETY: caller supplied a valid native window handle (NSView*,
        // HWND, X11 window id). vst2-host only forwards it to the
        // plugin's effEditOpen.
        let parent = unsafe { tutti_vst2_host::WindowHandle::from_ptr(parent_ptr) };
        let mut instance = self.inner.lock();
        instance
            .open_editor(parent)
            .map(|sz| EditorSize {
                width: sz.width,
                height: sz.height,
            })
            .map_err(|e| EditorError::PluginError(e.to_string()))
    }

    fn close_editor(&self) {
        self.inner.lock().close_editor();
    }

    fn editor_idle(&self) {
        let mut instance = self.inner.lock();
        instance.editor_idle();
        // Drain any plugin-internal parameter changes (knob movement on
        // the editor surface) and forward them to the user sink. Fires
        // on the GUI thread — same threading contract as the parameter
        // sink callback for the subprocess backend (which fires on the
        // bridge thread).
        for (index, value) in instance.drain_param_changes() {
            self.param_sink.fire(index as u32, value);
        }
    }

    fn save_state(&self) -> Option<Vec<u8>> {
        self.inner.lock().save_state().ok()
    }

    fn load_state(&self, data: &[u8]) {
        let _ = self.inner.lock().load_state(data);
    }

    fn parameters(&self) -> Option<Vec<ParameterInfo>> {
        let raw = self.inner.lock().parameters();
        Some(
            raw.into_iter()
                .map(|p| ParameterInfo {
                    id: p.id,
                    name: p.name,
                    unit: p.unit,
                    min_value: 0.0,
                    max_value: 1.0,
                    default_value: p.current as f64,
                    step_count: 0,
                    flags: crate::protocol::ParameterFlags {
                        automatable: true,
                        read_only: false,
                        wrap: false,
                        is_bypass: false,
                        hidden: false,
                    },
                })
                .collect(),
        )
    }

    fn parameter(&self, id: u32) -> Option<f32> {
        Some(self.inner.lock().parameter(id))
    }

    fn set_parameter_rt(&self, id: u32, value: f32) {
        // The audio thread can take this path (PluginHandle is shared);
        // use `try_lock` so we never block audio. Lost writes are
        // recoverable — the GUI thread will retry on the next idle.
        if let Some(instance) = self.inner.try_lock() {
            instance.set_parameter(id, value);
        }
    }

    fn is_crashed(&self) -> bool {
        // In-process: if the plugin crashed it took the host with it,
        // so a returning caller can never observe a crashed state.
        false
    }
}
