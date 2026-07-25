//! Host-side capability backend for the in-process WASM host.
//!
//! Implements [`HostParams`] and [`HostState`] — but deliberately **not**
//! `HostEditor`: the audio-plugin WIT world has no UI surface by design, so a
//! WASM plugin reports `PluginHandle::editor() == None` (its loader passes `None`
//! for the editor slot). There is no `open_editor → Err(GuiNotSupported)` stub —
//! absence is expressed by not implementing the trait.
//!
//! Holds the same `Arc<Mutex<WasmInstance>>` the audio unit holds. GUI thread
//! methods use blocking `lock()` — they wait at most one audio block since
//! `process_f32` releases the lock as soon as the guest call returns. The audio
//! thread always uses `try_lock` (in [`crate::audio_unit`]) and emits silence on
//! contention so a slow state-save call can't underrun audio.

use std::sync::Arc;

use parking_lot::Mutex;

use tutti_plugin::backend::{HostParams, HostState};
use tutti_plugin_types::ParameterInfo;

use crate::instance::WasmInstance;

pub(crate) struct InProcessWasmBackend {
    pub(crate) inner: Arc<Mutex<WasmInstance>>,
}

impl HostParams for InProcessWasmBackend {
    fn parameter_descriptors(&self) -> Option<Vec<ParameterInfo>> {
        Some(self.inner.lock().parameters().to_vec())
    }

    fn parameter_value(&self, id: u32) -> Option<f32> {
        Some(self.inner.lock().get_parameter(id) as f32)
    }

    fn set_parameter_value(&self, id: u32, value: f32) {
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

impl HostState for InProcessWasmBackend {
    fn save_state(&self) -> Option<Vec<u8>> {
        self.inner.lock().get_state().ok()
    }

    fn load_state(&self, data: &[u8]) {
        let _ = self.inner.lock().set_state(data);
    }
}
