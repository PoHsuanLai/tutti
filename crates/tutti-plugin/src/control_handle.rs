use crate::audio_node::{LatencyChangeSink, ParameterChangeSink};
use crate::control_backend::ControlBackend;
use crate::error::EditorError;
use crate::protocol::{ParameterInfo, PluginInfo};
use crate::window::{EditorCapabilities, EditorSize};
use raw_window_handle::HasWindowHandle;
use std::sync::Arc;
use tutti_midi_runtime::MidiSender;

/// Main-thread control handle for a loaded plugin.
///
/// Backend-agnostic — dispatches every method through the
/// `ControlBackend` trait, so out-of-process VST3/CLAP/AU and in-process
/// VST2 hosting share this surface. The backend owns whatever lifetime
/// guard keeps its plugin alive (subprocess `ProcessGuard` for the
/// out-of-process path; `Arc<Mutex<Vst2Instance>>` for in-process).
///
/// Clone is cheap (Arc-based). Action methods return `&Self` for chaining.
#[derive(Clone)]
pub struct PluginHandle {
    inner: Arc<dyn ControlBackend>,
    metadata: PluginInfo,
    latency_sink: LatencyChangeSink,
    param_sink: ParameterChangeSink,
    midi_sender: MidiSender,
}

impl PluginHandle {
    /// Construct from a `PluginClient` (out-of-process backend). Call
    /// this before moving the client into the fundsp graph.
    pub fn from_client(client: &crate::audio_node::PluginClient) -> Self {
        let backend = crate::bridge::SubprocessBackend::new(
            client.bridge(),
            Arc::clone(client.process_guard()),
        );
        Self {
            inner: Arc::new(backend),
            metadata: client.metadata().clone(),
            latency_sink: client.latency_sink().clone(),
            param_sink: client.param_sink().clone(),
            midi_sender: client.midi_sender(),
        }
    }

    /// Construct from any `ControlBackend` impl plus an explicit metadata
    /// snapshot. Used by every in-process loader (VST2, WASM).
    #[cfg(any(feature = "vst2-in-process", feature = "wasm"))]
    pub(crate) fn from_backend(
        inner: Arc<dyn ControlBackend>,
        metadata: PluginInfo,
        latency_sink: LatencyChangeSink,
        param_sink: ParameterChangeSink,
        midi_sender: MidiSender,
    ) -> Self {
        Self {
            inner,
            metadata,
            latency_sink,
            param_sink,
            midi_sender,
        }
    }

    #[cfg(test)]
    pub(crate) fn from_bridge_and_metadata(
        bridge: Arc<crate::bridge::PluginBridge>,
        metadata: PluginInfo,
    ) -> Self {
        let guard = Arc::new(crate::audio_node::ProcessGuard::for_test(
            crate::config::BridgeConfig::default(),
        ));
        let backend = crate::bridge::SubprocessBackend::new(bridge, guard);
        let (sender, _receiver) =
            tutti_midi_runtime::MidiEventSlot::pair(tutti_core::midi::MidiUnitId::next());
        Self {
            inner: Arc::new(backend),
            metadata,
            latency_sink: LatencyChangeSink::default(),
            param_sink: ParameterChangeSink::default(),
            midi_sender: sender,
        }
    }

    pub fn has_editor(&self) -> bool {
        self.metadata.has_editor
    }

    /// Embed the plugin's editor into `parent`. Pass anything that impls
    /// [`HasWindowHandle`] — Bevy windows, winit windows, wgpu surfaces.
    ///
    /// Returns the editor's requested size on success; otherwise a
    /// structured [`EditorError`] describing why (plugin crashed, GUI
    /// feature not compiled in, platform not supported, plugin rejected
    /// the call).
    pub fn open_editor(&self, parent: impl HasWindowHandle) -> Result<EditorSize, EditorError> {
        let raw = parent
            .window_handle()
            .map_err(|e| EditorError::PluginError(format!("failed to get window handle: {e}")))?
            .as_raw();
        let ptr = crate::window::extract_platform_ptr(raw)?;
        self.inner.open_editor(ptr)
    }

    pub fn close_editor(&self) -> &Self {
        self.inner.close_editor();
        self
    }

    /// Call periodically (~30Hz) while editor is open.
    pub fn editor_idle(&self) -> &Self {
        self.inner.editor_idle();
        self
    }

    /// Call after `open_editor`.
    pub fn editor_capabilities(&self) -> EditorCapabilities {
        self.inner.editor_capabilities()
    }

    /// Returns the snapped/clamped size the plugin applied.
    pub fn set_editor_size(
        &self,
        requested: EditorSize,
    ) -> Result<EditorSize, EditorError> {
        self.inner.set_editor_size(requested)
    }

    pub fn poll_editor_resize_request(&self) -> Option<EditorSize> {
        self.inner.poll_editor_resize_request()
    }

    pub fn save_state(&self) -> Option<Vec<u8>> {
        self.inner.save_state()
    }

    pub fn load_state(&self, data: &[u8]) -> &Self {
        self.inner.load_state(data);
        self
    }

    pub fn parameters(&self) -> Option<Vec<ParameterInfo>> {
        self.inner.parameters()
    }

    pub fn parameter(&self, param_id: u32) -> Option<f32> {
        self.inner.parameter(param_id)
    }

    /// RT-safe, fire-and-forget.
    pub fn set_parameter(&self, param_id: u32, value: f32) -> &Self {
        self.inner.set_parameter_rt(param_id, value);
        self
    }

    pub fn metadata(&self) -> &PluginInfo {
        &self.metadata
    }

    pub fn name(&self) -> &str {
        &self.metadata.name
    }

    /// Producer handle for this plugin's MIDI inbox. Cheap to clone —
    /// `MidiSender` is `Arc`-backed. Send `MidiEvent`s through the
    /// returned sender; the audio thread polls them on the next
    /// process call.
    pub fn midi_sender(&self) -> MidiSender {
        self.midi_sender.clone()
    }

    pub fn is_crashed(&self) -> bool {
        self.inner.is_crashed()
    }

    /// Register a callback invoked whenever the plugin reports a new
    /// latency. Fires on the bridge thread for the out-of-process
    /// backend; on the GUI thread (during `editor_idle`) for the
    /// in-process VST2 backend. Move heavy work to another thread before
    /// touching graph state. Replaces any previous callback.
    pub fn on_latency_changed<F: Fn(usize) + Send + Sync + 'static>(&self, f: F) -> &Self {
        self.latency_sink.set(f);
        self
    }

    /// Register a callback invoked when the plugin writes back a
    /// parameter value internally (preset load, automation, host
    /// write-back). See [`Self::on_latency_changed`] for thread caveats.
    /// Replaces any previous callback.
    pub fn on_parameter_changed<F: Fn(u32, f32) + Send + Sync + 'static>(&self, f: F) -> &Self {
        self.param_sink.set(f);
        self
    }

    /// Clear the latency-changed callback (if any).
    pub fn clear_latency_callback(&self) -> &Self {
        self.latency_sink.clear();
        self
    }

    /// Clear the parameter-changed callback (if any).
    pub fn clear_parameter_callback(&self) -> &Self {
        self.param_sink.clear();
        self
    }
}
