//! `ControlBackend` impl for the in-process WASM host.
//!
//! Holds the same `Arc<Mutex<WasmInstance>>` the audio unit holds.
//! GUI thread methods use blocking `lock()` — they wait at most one
//! audio block since `process_f32` releases the lock as soon as the
//! guest call returns. The audio thread always uses `try_lock` (in
//! [`crate::audio_unit`]) and emits silence on contention so a slow
//! state-save call can't underrun audio.

use std::ffi::c_void;
use std::sync::Arc;

use parking_lot::Mutex;

use tutti_plugin::backend::ControlBackend;
use tutti_plugin_types::{EditorError, EditorSize, ParameterInfo};

use crate::instance::WasmInstance;

pub(crate) struct InProcessWasmBackend {
    pub(crate) inner: Arc<Mutex<WasmInstance>>,
}

impl ControlBackend for InProcessWasmBackend {
    fn open_editor(
        &self,
        _parent_ptr: *mut c_void,
    ) -> std::result::Result<EditorSize, EditorError> {
        // The audio-plugin WIT world has no UI surface by design. Plugins
        // that want a UI ship a sibling editor extension that talks to
        // the host via the parameter API; see examples/wasm-plugins/
        // synth-with-ui for the split-component pattern.
        Err(EditorError::GuiNotSupported {
            format: "wasm".to_string(),
        })
    }

    fn close_editor(&self) {}

    fn editor_idle(&self) {}

    fn save_state(&self) -> Option<Vec<u8>> {
        self.inner.lock().get_state().ok()
    }

    fn load_state(&self, data: &[u8]) {
        let _ = self.inner.lock().set_state(data);
    }

    fn parameters(&self) -> Option<Vec<ParameterInfo>> {
        Some(self.inner.lock().parameters().to_vec())
    }

    fn parameter(&self, id: u32) -> Option<f32> {
        Some(self.inner.lock().get_parameter(id) as f32)
    }

    fn set_parameter_rt(&self, id: u32, value: f32) {
        // Audio thread can call this path; `try_lock` so we never
        // block. Lost writes are tolerable — the GUI thread typically
        // re-asserts the value, and parameter automation on the audio
        // thread holds the same lock as `process` already.
        if let Some(mut instance) = self.inner.try_lock() {
            instance.set_parameter(id, value as f64);
        }
    }

    fn is_crashed(&self) -> bool {
        // In-process: a hard crash in the guest takes the host down too,
        // so a returning caller never sees `true`. A guest *trap*
        // surfaces as a `process_f32` error and the audio thread emits
        // silence — but the store is poisoned and subsequent calls
        // also error. v0.2: track an `AtomicBool` poisoned flag and
        // expose it here, then drive re-instantiation from the UI.
        false
    }
}
