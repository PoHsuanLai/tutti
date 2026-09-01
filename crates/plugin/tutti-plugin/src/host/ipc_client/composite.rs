//! Plugin bridge — composites out-of-process audio with in-process GUI.

use super::audio::{AudioBridge, BridgeListener, BridgeThread, HarmonyInputs};
use crate::error::{Delivered, EditorError, Result, StateError};
use crate::format::gui::PluginEditor;
use crate::protocol::{
    MidiEventVec, Normalized, NoteExpressionChanges, ParamAddress, ParameterChanges, ParameterInfo,
    Preset, PresetId, TransportInfo,
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
    ///
    /// `sample_rate` seeds the bridge thread's per-block reply timeout (see
    /// `dispatch::process_timeout`); `set_sample_rate_rt` keeps it current after
    /// device rate changes. The audio thread itself sizes nothing from the rate
    /// any more — it does not wait.
    pub(crate) fn new(
        socket_path: PathBuf,
        audio_buffer: Arc<AudioSlab>,
        plugin_path: PathBuf,
        sample_rate: f64,
    ) -> Result<(Arc<Self>, BridgeThread)> {
        let (audio, bridge_thread) = AudioBridge::new(socket_path, audio_buffer, sample_rate)?;
        let bridge = Arc::new(Self {
            audio,
            plugin_path,
            gui: Mutex::new(None),
        });
        Ok((bridge, bridge_thread))
    }

    /// Hand block `seq` to the bridge without waiting. See
    /// [`AudioBridge::submit`] — returning `true` means the block was accepted,
    /// never that its output is ready.
    pub fn submit(
        &self,
        seq: u64,
        num_samples: usize,
        midi_events: MidiEventVec,
        param_changes: ParameterChanges,
        note_expression: NoteExpressionChanges,
        harmony: HarmonyInputs,
        transport: TransportInfo,
        midi_out: &mut MidiEventVec,
    ) -> bool {
        self.audio.submit(
            seq,
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

    pub fn set_parameter_rt(&self, param_id: ParamAddress, value: Normalized) -> bool {
        // Cosmetic GUI mirror (keeps the display in sync): `try_lock`, never
        // block. The `_rt` contract must stay non-blocking — a blocking
        // `lock()` here could stall the caller behind a multi-millisecond
        // main-thread `open_editor`/`editor_idle` holding the same GUI lock. The
        // authoritative delivery is the audio command below; a dropped GUI mirror
        // self-heals on the next edit / editor idle.
        if let Ok(mut guard) = self.gui.try_lock() {
            if let Some(gui) = guard.as_mut() {
                gui.set_parameter(param_id, value);
            }
        }
        // The wire carries `f32`; the domain is enforced up to this point.
        self.audio.set_parameter_rt(param_id, value.get() as f32)
    }

    pub fn set_automation_state_rt(&self, mode: crate::protocol::AutomationMode) -> Delivered {
        // Deliver to BOTH the audio subprocess AND the in-process GUI instance
        // (mirrors `set_parameter_rt`). The automation-state advisory drives
        // editor UI feedback (a glowing knob ring), which lives in the GUI
        // instance — so a GUI-only delivery would leave it unlit. The audio
        // instance also receives it for formats that gate DSP on it. Both sides
        // take the format-neutral `AutomationMode` and encode it at their own
        // ABI edge (the GUI instance and the server loader) — no format bitmask
        // here.
        //
        // The GUI mirror is cosmetic, so `try_lock` (never block): the `_rt`
        // contract stays non-blocking, and a dropped glow self-heals on the next
        // mode change. See `set_parameter_rt` for the RT rationale.
        if let Ok(mut guard) = self.gui.try_lock() {
            if let Some(gui) = guard.as_mut() {
                gui.set_automation_state(mode);
            }
        }
        self.audio.set_automation_state_rt(mode)
    }

    pub fn set_sample_rate_rt(&self, rate: f64) -> bool {
        self.audio.set_sample_rate_rt(rate)
    }

    /// Audio instance only, unlike `set_automation_state_rt`.
    ///
    /// The render mode changes how the plugin *processes*; it drives no editor
    /// feedback, so the GUI mirror has nothing to show. Sending it there would
    /// also mean re-initializing the GUI instance on three of the four formats
    /// for no visible effect.
    pub fn set_render_mode_rt(&self, mode: crate::protocol::RenderMode) -> bool {
        self.audio.set_render_mode_rt(mode)
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

    /// Why the bridge died, or `None` while it is alive. See
    /// [`AudioBridge::crash_cause`](super::audio::AudioBridge::crash_cause).
    pub fn crash_cause(&self) -> Option<String> {
        self.audio.crash_cause()
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

    /// [`open_editor`](Self::open_editor) for a plugin-owned floating window.
    ///
    /// Same lazy GUI load and the same post-open state sync — the editor
    /// instance and its state are the plugin's either way. What differs is only
    /// that no parent goes in and no size comes out.
    pub fn open_floating_editor(&self) -> std::result::Result<(), EditorError> {
        tutti_plugin_types::assert_main_thread();
        if self.audio.is_crashed() {
            return Err(EditorError::PluginCrashed);
        }

        let mut guard = self.gui.lock().map_err(|_| EditorError::Busy)?;
        if guard.is_none() {
            let instance =
                crate::format::gui::load_gui_instance(&self.plugin_path).map_err(|e| {
                    EditorError::PluginError(format!("failed to load in-process GUI: {e}"))
                })?;
            *guard = Some(instance);
        }
        let gui = guard.as_mut().expect("just inserted");

        // Read before the open, as the embedded path does: audio is the source
        // of truth for state, and some plugins refuse `set_state` until their
        // view exists.
        let state_to_sync = self.audio.save_state();

        gui.open_floating_editor()
            .map_err(|e| EditorError::PluginError(e.to_string()))?;

        if let Some(state) = state_to_sync {
            let _ = gui.set_state(&state);
        }

        Ok(())
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

    /// The **audio** instance's answer is the one returned.
    ///
    /// The GUI instance is a second dlopen of the same plugin kept only so its
    /// editor displays the right values; a failure to mirror there leaves the
    /// editor stale but the audio correct, which is not what a caller asking
    /// "did my preset load" is asking about. Mirroring stays best-effort, and
    /// the audio result is the return value.
    pub fn load_state(&self, data: &[u8]) -> std::result::Result<(), StateError> {
        let audio = self.audio.load_state(data);
        // Also load into GUI instance so its display stays in sync.
        if let Ok(mut guard) = self.gui.lock() {
            if let Some(gui) = guard.as_mut() {
                let _ = gui.set_state(data);
            }
        }
        audio
    }

    pub fn parameters(&self) -> Option<Vec<ParameterInfo>> {
        self.audio.parameters()
    }

    pub fn parameter(&self, param_id: ParamAddress) -> Option<f32> {
        self.audio.parameter(param_id)
    }

    pub fn parameter_text(&self, param_id: ParamAddress, value: Normalized) -> Option<String> {
        self.audio.parameter_text(param_id, value)
    }

    pub fn parameter_value_from_text(
        &self,
        param_id: ParamAddress,
        text: &str,
    ) -> Option<Normalized> {
        self.audio.parameter_value_from_text(param_id, text)
    }

    pub fn presets(&self) -> Option<Vec<Preset>> {
        self.audio.presets()
    }

    /// Load a preset in the audio instance.
    ///
    /// Unlike [`load_state`](Self::load_state) this does **not** mirror into
    /// the GUI instance: `load_state` pushes host-held bytes into both, but a
    /// preset load is the plugin reading its own file, and the editor's copy
    /// has no such file to read. A plugin whose editor shows a stale name
    /// after this reports it through the existing
    /// `PluginParamValuesChanged` / `PluginParamTitlesChanged` refresh path,
    /// which is the mechanism for exactly this.
    pub fn load_preset(&self, id: PresetId) -> bool {
        self.audio.load_preset(id)
    }

    pub fn current_preset(&self) -> Option<PresetId> {
        self.audio.current_preset()
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

    fn parameter_value(&self, id: ParamAddress) -> Option<f32> {
        self.bridge.parameter(id)
    }

    fn parameter_text(&self, id: ParamAddress, value: Normalized) -> Option<String> {
        self.bridge.parameter_text(id, value)
    }

    /// Asked of the audio instance, like every other parameter read here.
    ///
    /// On VST2 this also *writes* that instance (`effString2Parameter` parses by
    /// applying), which is the same instance `set_parameter_value` targets — so
    /// the value lands where a knob poke would, and the GUI mirror follows
    /// through the existing `PluginParamValuesChanged` refresh rather than a
    /// second write from here.
    fn parameter_value_from_text(&self, id: ParamAddress, text: &str) -> Option<Normalized> {
        self.bridge.parameter_value_from_text(id, text)
    }

    fn set_parameter_value(&self, id: ParamAddress, value: Normalized) {
        // Stays `Normalized` all the way to the bridge, which narrows to the
        // `f32` the wire carries. The subprocess re-clamps on receipt — it
        // must, since the wire is a foreign boundary and the peer is another
        // process — but the value leaving here is already on the unit interval.
        self.bridge.set_parameter_rt(id, value);
    }

    fn is_crashed(&self) -> bool {
        self.bridge.is_crashed()
    }

    fn crash_cause(&self) -> Option<String> {
        self.bridge.crash_cause()
    }
}

impl crate::host::handles::capabilities::HostState for SubprocessBackend {
    fn save_state(&self) -> Option<Vec<u8>> {
        self.bridge.save_state()
    }

    fn load_state(&self, data: &[u8]) -> std::result::Result<(), StateError> {
        self.bridge.load_state(data)
    }
}

impl crate::host::handles::capabilities::HostEditor for SubprocessBackend {
    fn open_editor(&self, parent_ptr: *mut c_void) -> std::result::Result<EditorSize, EditorError> {
        self.bridge.open_editor(parent_ptr)
    }

    fn open_floating_editor(&self) -> std::result::Result<(), EditorError> {
        self.bridge.open_floating_editor()
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

impl crate::host::handles::capabilities::HostAutomationState for SubprocessBackend {
    fn set_automation_mode(
        &self,
        mode: crate::protocol::AutomationMode,
    ) -> std::result::Result<(), EditorError> {
        if self.bridge.is_crashed() {
            return Err(EditorError::PluginCrashed);
        }
        // Delivered to both the audio subprocess and the in-process GUI (the
        // knob-glow lives in the GUI). [`Delivered`] says whether the command
        // was *queued*; no format confirms the plugin visibly reacted. The
        // format-neutral `AutomationMode` flows all the way to each ABI edge,
        // which encodes it.
        automation_push_outcome(self.bridge.set_automation_state_rt(mode))
    }
}

/// Turn a queue outcome into the error a UI shows.
///
/// A free function so it is reachable from a test: building a
/// [`SubprocessBackend`] needs a live subprocess, and the interesting behaviour
/// here is the mapping, not the plumbing.
///
/// The two failures produce **different** errors because they call for
/// different responses. Collapsed into one "not delivered" message, a UI could
/// report the failure but never tell the user whether retrying was worth it.
fn automation_push_outcome(delivered: Delivered) -> std::result::Result<(), EditorError> {
    match delivered {
        Delivered::Yes => Ok(()),
        Delivered::Dropped => Err(EditorError::PluginError(
            "automation-state push dropped: the command queue is full; \
             the next change should land"
                .into(),
        )),
        Delivered::PluginDead => Err(EditorError::PluginCrashed),
    }
}

impl crate::host::handles::capabilities::HostPresets for SubprocessBackend {
    fn presets(&self) -> Vec<Preset> {
        // A crashed subprocess yields `None`; flattened to an empty list
        // because the trait's contract is "what the plugin advertises", and a
        // caller distinguishing "cannot ask" from "listed nothing" reads
        // `Features::PRESET_LIST` rather than a sentinel here.
        self.bridge.presets().unwrap_or_default()
    }

    fn load_preset(&self, id: &PresetId) -> bool {
        !self.bridge.is_crashed() && self.bridge.load_preset(id.clone())
    }

    fn current_preset(&self) -> Option<PresetId> {
        self.bridge.current_preset()
    }
}

impl crate::host::handles::capabilities::HostRenderMode for SubprocessBackend {
    fn set_render_mode(&self, mode: crate::protocol::RenderMode) -> bool {
        !self.bridge.is_crashed() && self.bridge.set_render_mode_rt(mode)
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

#[cfg(test)]
mod automation_outcome_tests {
    use super::*;

    /// A dropped push and a dead plugin must not produce the same error.
    ///
    /// This is the property the `bool` could not express, and the only reason
    /// [`Delivered`] exists rather than a two-state answer: one is transient and
    /// worth retrying, the other is permanent. Asserting they *differ* is what
    /// fails if someone later folds the two arms back together — an assertion
    /// on either arm alone would survive that.
    #[test]
    fn a_dropped_push_and_a_dead_plugin_report_differently() {
        let dropped = automation_push_outcome(Delivered::Dropped)
            .expect_err("a dropped push is not a success");
        let dead = automation_push_outcome(Delivered::PluginDead)
            .expect_err("a dead plugin is not a success");

        assert!(
            matches!(dropped, EditorError::PluginError(_)),
            "a full queue is the plugin declining to be reached, not a crash: {dropped:?}"
        );
        assert!(
            matches!(dead, EditorError::PluginCrashed),
            "a dead plugin must report as crashed: {dead:?}"
        );
        assert_ne!(
            dropped.to_string(),
            dead.to_string(),
            "the two failures must be distinguishable by a user reading the message"
        );
    }

    /// The negative half: a delivered push is not reported as a failure. Without
    /// this, folding every arm to `Err` would still pass the test above.
    #[test]
    fn a_delivered_push_is_not_an_error() {
        assert!(automation_push_outcome(Delivered::Yes).is_ok());
    }
}
