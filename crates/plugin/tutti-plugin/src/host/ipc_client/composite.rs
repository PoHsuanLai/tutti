//! Plugin bridge — composites out-of-process audio with in-process GUI.

use super::audio::{AudioBridge, BridgeListener, BridgeThread, HarmonyInputs};
use crate::error::{EditorError, Result};
use crate::format::gui::PluginEditor;
use crate::protocol::{
    MidiEventVec, NoteExpressionChanges, ParameterChanges, ParameterInfo, TransportInfo,
};
use crate::util::transport::shm::AudioSlab;
use crate::util::window::{EditorCapabilities, EditorSize, WindowHandle};
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Composites an out-of-process audio bridge with a lazily-loaded in-process
/// GUI instance. Audio processing goes through `AudioBridge` (IPC to
/// plugin-server child process). Editor/GUI operations load the plugin
/// in-process on first `open_editor` call.
pub struct PluginBridge {
    audio: AudioBridge,
    plugin_path: PathBuf,
    gui: Mutex<Option<Box<dyn PluginEditor>>>,
}

impl PluginBridge {
    /// Build the full bridge — spawns the audio bridge thread that connects
    /// to the plugin-server at `socket_path`, wires it to `audio_buffer` for
    /// block-oriented audio transfer, and leaves the GUI instance to be
    /// lazily loaded on first `open_editor`.
    ///
    /// Returns an `Arc<Self>` (for cheap cloning into the audio graph) and
    /// a `BridgeThread` (whose `Drop` shuts down the bridge thread).
    pub(crate) fn new(
        socket_path: PathBuf,
        audio_buffer: Arc<AudioSlab>,
        plugin_path: PathBuf,
    ) -> Result<(Arc<Self>, BridgeThread)> {
        let (audio, bridge_thread) = AudioBridge::new(socket_path, audio_buffer)?;
        let bridge = Arc::new(Self {
            audio,
            plugin_path,
            gui: Mutex::new(None),
        });
        Ok((bridge, bridge_thread))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn process(
        &self,
        num_samples: usize,
        midi_events: MidiEventVec,
        param_changes: ParameterChanges,
        note_expression: NoteExpressionChanges,
        harmony: HarmonyInputs,
        transport: TransportInfo,
        midi_out: &mut MidiEventVec,
    ) -> bool {
        self.audio.process(
            num_samples,
            midi_events,
            param_changes,
            note_expression,
            harmony,
            transport,
            midi_out,
        )
    }

    /// Install a listener for plugin-originated unsolicited events
    /// (latency changes, parameter write-backs). Invoked on the bridge
    /// thread. Pass `None` to clear.
    pub fn set_listener(&self, listener: Option<BridgeListener>) {
        self.audio.set_listener(listener);
    }

    pub fn set_parameter_rt(&self, param_id: u32, value: f32) -> bool {
        // Also sync to GUI instance if loaded (keeps display in sync).
        if let Ok(mut guard) = self.gui.lock() {
            if let Some(gui) = guard.as_mut() {
                gui.set_parameter(param_id, value as f64);
            }
        }
        self.audio.set_parameter_rt(param_id, value)
    }

    pub fn set_automation_state_rt(&self, state: i32) -> bool {
        self.audio.set_automation_state_rt(state)
    }

    pub fn set_sample_rate_rt(&self, rate: f64) -> bool {
        self.audio.set_sample_rate_rt(rate)
    }

    pub fn reset_rt(&self) -> bool {
        self.audio.reset_rt()
    }

    pub fn audio_buffer(&self) -> &Arc<AudioSlab> {
        self.audio.audio_buffer()
    }

    pub fn is_crashed(&self) -> bool {
        self.audio.is_crashed()
    }

    pub fn open_editor(
        &self,
        parent_ptr: *mut c_void,
    ) -> std::result::Result<EditorSize, EditorError> {
        tutti_plugin_types::assert_main_thread();
        if self.audio.is_crashed() {
            return Err(EditorError::PluginCrashed);
        }

        tracing::info!(
            "[bridge] open_editor: acquiring GUI lock for {}",
            self.plugin_path.display()
        );
        let mut guard = self.gui.lock().map_err(|_| EditorError::Busy)?;

        // Lazy-load the in-process GUI instance on first open.
        if guard.is_none() {
            tracing::info!("[bridge] open_editor: lazy-loading in-process GUI");
            let instance =
                crate::format::gui::load_gui_instance(&self.plugin_path).map_err(|e| {
                    EditorError::PluginError(format!("failed to load in-process GUI: {e}"))
                })?;
            tracing::info!("[bridge] open_editor: in-process GUI loaded successfully");
            *guard = Some(instance);
        }

        let gui = guard.as_mut().expect("just inserted");

        // Sync state from audio (source of truth) to GUI after opening.
        // Some plugins crash if set_state is called before the editor is attached.
        let state_to_sync = self.audio.save_state();

        tracing::info!("[bridge] open_editor: calling gui.open_editor(parent={parent_ptr:?})");
        // SAFETY: `parent_ptr` is the platform-native window pointer
        // extracted by `extract_platform_ptr` from a caller-supplied
        // `RawWindowHandle`, which is alive for the open_editor call.
        let size = gui
            .open_editor(unsafe { WindowHandle::from_ptr(parent_ptr) })
            .map_err(|e| EditorError::PluginError(e.to_string()))?;
        tracing::info!(
            "[bridge] open_editor: editor opened {}x{}",
            size.width,
            size.height
        );

        // Sync state after editor is open (some plugins need the view attached first).
        if let Some(state) = state_to_sync {
            tracing::info!(
                "[bridge] open_editor: syncing {} bytes of state post-open",
                state.len()
            );
            let _ = gui.set_state(&state);
        }

        Ok(size)
    }

    pub fn close_editor(&self) -> bool {
        tutti_plugin_types::assert_main_thread();
        self.close_editor_inner()
    }

    /// Editor-close without the main-thread assert. Used by `Drop`, which can
    /// run on a worker thread when the fundsp graph releases a plugin node
    /// during a graph rebuild (`commit_graph` runs off the main thread).
    /// Asserting there would false-fire on a teardown that is benign — the
    /// public [`close_editor`](Self::close_editor) keeps the guard for the
    /// real UI-thread call path.
    fn close_editor_inner(&self) -> bool {
        let Ok(mut guard) = self.gui.lock() else {
            return false;
        };
        if let Some(gui) = guard.as_mut() {
            // Flush pending GUI param changes before closing.
            for (param_id, value) in gui.poll_gui_param_changes() {
                self.audio.set_parameter_rt(param_id, value);
            }
            gui.close_editor();
            true
        } else {
            false
        }
    }

    pub fn editor_idle(&self) {
        tutti_plugin_types::assert_main_thread();
        if let Ok(mut guard) = self.gui.lock() {
            if let Some(gui) = guard.as_mut() {
                gui.editor_idle();
                // Forward GUI-originated parameter changes to the audio bridge.
                for (param_id, value) in gui.poll_gui_param_changes() {
                    self.audio.set_parameter_rt(param_id, value);
                }
            }
        }
    }

    pub fn save_state(&self) -> Option<Vec<u8>> {
        self.audio.save_state()
    }

    pub fn load_state(&self, data: &[u8]) -> bool {
        let audio_ok = self.audio.load_state(data);
        // Also load into GUI instance so its display stays in sync.
        if let Ok(mut guard) = self.gui.lock() {
            if let Some(gui) = guard.as_mut() {
                let _ = gui.set_state(data);
            }
        }
        audio_ok
    }

    pub fn parameters(&self) -> Option<Vec<ParameterInfo>> {
        self.audio.parameters()
    }

    pub fn parameter(&self, param_id: u32) -> Option<f32> {
        self.audio.parameter(param_id)
    }

    pub fn editor_capabilities(&self) -> EditorCapabilities {
        let Ok(mut guard) = self.gui.lock() else {
            return EditorCapabilities::default();
        };
        match guard.as_mut() {
            Some(gui) => gui.editor_capabilities(),
            None => EditorCapabilities::default(),
        }
    }

    pub fn set_editor_size(
        &self,
        requested: EditorSize,
    ) -> std::result::Result<EditorSize, EditorError> {
        tutti_plugin_types::assert_main_thread();
        let mut guard = self.gui.lock().map_err(|_| EditorError::Busy)?;
        let gui = guard
            .as_mut()
            .ok_or_else(|| EditorError::PluginError("editor not open".into()))?;
        gui.set_editor_size(requested)
            .map_err(|e| EditorError::PluginError(e.to_string()))
    }

    pub fn poll_editor_resize_request(&self) -> Option<EditorSize> {
        let mut guard = self.gui.lock().ok()?;
        guard.as_mut()?.poll_editor_resize_request()
    }
}

/// Host-side capability backend for the out-of-process VST3 / CLAP / AU path —
/// implements [`HostParams`](crate::host::handles::capabilities::HostParams),
/// [`HostState`](crate::host::handles::capabilities::HostState), and
/// [`HostEditor`](crate::host::handles::capabilities::HostEditor).
///
/// Bundles the `PluginBridge` (audio IPC + lazy in-process GUI loader)
/// with the subprocess lifetime guard so dropping this backend tears
/// down both. The guard is `Arc`-shared with the `PluginClient` audio
/// node so the subprocess only dies once both the audio node and all
/// handles are gone.
pub(crate) struct SubprocessBackend {
    bridge: Arc<PluginBridge>,
    _guard: Arc<crate::host::node::ProcessGuard>,
}

impl SubprocessBackend {
    pub(crate) fn new(
        bridge: Arc<PluginBridge>,
        guard: Arc<crate::host::node::ProcessGuard>,
    ) -> Self {
        Self {
            bridge,
            _guard: guard,
        }
    }
}

impl crate::host::handles::capabilities::HostParams for SubprocessBackend {
    fn parameter_descriptors(&self) -> Option<Vec<ParameterInfo>> {
        self.bridge.parameters()
    }

    fn parameter_value(&self, id: u32) -> Option<f32> {
        self.bridge.parameter(id)
    }

    fn set_parameter_value(&self, id: u32, value: f32) {
        self.bridge.set_parameter_rt(id, value);
    }

    fn is_crashed(&self) -> bool {
        self.bridge.is_crashed()
    }
}

impl crate::host::handles::capabilities::HostState for SubprocessBackend {
    fn save_state(&self) -> Option<Vec<u8>> {
        self.bridge.save_state()
    }

    fn load_state(&self, data: &[u8]) {
        self.bridge.load_state(data);
    }
}

impl crate::host::handles::capabilities::HostEditor for SubprocessBackend {
    fn open_editor(&self, parent_ptr: *mut c_void) -> std::result::Result<EditorSize, EditorError> {
        self.bridge.open_editor(parent_ptr)
    }

    fn close_editor(&self) {
        self.bridge.close_editor();
    }

    fn editor_idle(&self) {
        self.bridge.editor_idle();
    }

    fn editor_capabilities(&self) -> EditorCapabilities {
        self.bridge.editor_capabilities()
    }

    fn set_editor_size(
        &self,
        requested: EditorSize,
    ) -> std::result::Result<EditorSize, EditorError> {
        self.bridge.set_editor_size(requested)
    }

    fn poll_editor_resize_request(&self) -> Option<EditorSize> {
        self.bridge.poll_editor_resize_request()
    }
}

impl Drop for PluginBridge {
    fn drop(&mut self) {
        // Close the editor, then intentionally leak the GUI instance.
        //
        // JUCE plugins crash during their ScopedJuceInitialiser_GUI destructor
        // because Desktop::~Desktop() tries to unregister notification observers
        // that reference already-freed CFString objects. This is a known issue
        // with JUCE-based AU/CLAP plugins — the JUCE static cleanup order is
        // incompatible with being hosted as a dynamic library.
        //
        // Leaking the GUI instance avoids the crash. The OS reclaims all memory
        // on process exit anyway.
        //
        // Use the *_inner variant: Drop can run on a worker thread (the fundsp
        // graph releases this node during commit_graph, which runs off-main),
        // so the main-thread assert in the public close_editor would false-fire.
        self.close_editor_inner();
        if let Ok(mut guard) = self.gui.lock() {
            if let Some(gui) = guard.take() {
                std::mem::forget(gui);
            }
        }
    }
}
