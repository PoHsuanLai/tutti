//! Plugin bridge — composites out-of-process audio with in-process GUI.

use super::audio::{AudioBridge, BridgeListener, BridgeThread, HarmonyInputs, Submitted};
use crate::error::{Delivered, EditorError, Result, StateError};
use crate::format::gui::PluginEditor;
use crate::protocol::{
    MidiEventVec, Normalized, NoteExpressionChanges, ParamId, ParameterChanges, ParameterInfo,
    Preset, PresetId, TransportInfo,
};
use crate::util::transport::shm::AudioSlab;
use crate::util::window::{ContentScale, EditorCapabilities, EditorSize, WindowHandle};
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tutti_plugin_types::{Edit, EditDrain, EditSink, MainThread, edits};
use tutti_types::SampleRate;

/// Composites an out-of-process audio bridge with a lazily-loaded in-process
/// GUI instance. Audio processing goes through `AudioBridge` (IPC to
/// plugin-server child process). Editor/GUI operations load the plugin
/// in-process on first `open_editor` call.
pub struct PluginBridge {
    audio: AudioBridge,
    plugin_path: PathBuf,
    gui: Mutex<Option<Box<dyn PluginEditor>>>,
    /// What the realtime setters want the in-process editor to show, applied by
    /// the next [`editor_idle`](Self::editor_idle).
    mirror: GuiMirror,
}

/// How many parameter writes wait for the next editor idle. A full ring drops
/// the newest: the mirror is cosmetic, and the audio command is authoritative.
const MIRROR_CAPACITY: usize = 256;

/// The cosmetic mirror of the realtime setters onto the in-process editor.
///
/// The in-process editor is a second instance of the plugin whose parameter and
/// automation calls are main-thread calls, and `set_parameter_rt` and
/// `set_automation_state_rt` run wherever the node's controls are written. So
/// they queue here (never blocking, never allocating) and the main-thread
/// [`editor_idle`](PluginBridge::editor_idle) applies what was queued.
struct GuiMirror {
    params: Mutex<EditSink>,
    drain: Mutex<EditDrain>,
    automation: Mutex<Option<crate::protocol::AutomationMode>>,
    /// Whether an editor is open. Nothing is queued while it is not, and what
    /// was queued is discarded when it opens or closes: an edit made with no
    /// editor to show it is not one to replay on the first idle.
    open: AtomicBool,
}

impl GuiMirror {
    fn new() -> Self {
        let (sink, drain) = edits(MIRROR_CAPACITY);
        Self {
            params: Mutex::new(sink),
            drain: Mutex::new(drain),
            automation: Mutex::new(None),
            open: AtomicBool::new(false),
        }
    }

    /// Queues a parameter write for the editor, if one is open. Never blocks.
    fn push_param(&self, id: ParamId, value: Normalized) {
        if self.open.load(Ordering::Acquire)
            && let Ok(mut sink) = self.params.try_lock()
        {
            let _ = sink.send(Edit::Param { id, value });
        }
    }

    /// Queues the latest automation mode for the editor, if one is open.
    fn push_automation(&self, mode: crate::protocol::AutomationMode) {
        if self.open.load(Ordering::Acquire)
            && let Ok(mut latest) = self.automation.try_lock()
        {
            *latest = Some(mode);
        }
    }

    /// Drops everything queued.
    fn discard(&self) {
        if let Ok(mut drain) = self.drain.lock() {
            drain.drain(|_| {});
        }
        if let Ok(mut latest) = self.automation.lock() {
            *latest = None;
        }
    }

    /// An editor opened (after its state sync): what was queued before it is
    /// stale, and writes are queued from here on.
    fn opened(&self) {
        self.open.store(false, Ordering::Release);
        self.discard();
        self.open.store(true, Ordering::Release);
    }

    /// The editor closed: stop queueing and drop what waits.
    fn closed(&self) {
        self.open.store(false, Ordering::Release);
        self.discard();
    }
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
        sample_rate: SampleRate,
    ) -> Result<(Arc<Self>, BridgeThread)> {
        let (audio, bridge_thread) = AudioBridge::new(socket_path, audio_buffer, sample_rate)?;
        let bridge = Arc::new(Self {
            audio,
            plugin_path,
            gui: Mutex::new(None),
            mirror: GuiMirror::new(),
        });
        Ok((bridge, bridge_thread))
    }

    /// As [`new`](Self::new), with a shortened state-transfer progress deadline.
    ///
    /// Exists so the stall path can be asserted in milliseconds rather than by
    /// sleeping out the real deadline. Set before the `Arc` is formed because
    /// that is the last moment the bridge is uniquely owned.
    #[cfg(test)]
    pub(crate) fn new_with_state_progress_timeout(
        socket_path: PathBuf,
        audio_buffer: Arc<AudioSlab>,
        plugin_path: PathBuf,
        sample_rate: SampleRate,
        state_progress_timeout: std::time::Duration,
    ) -> Result<(Arc<Self>, BridgeThread)> {
        let (mut audio, bridge_thread) = AudioBridge::new(socket_path, audio_buffer, sample_rate)?;
        audio.set_state_progress_timeout(state_progress_timeout);
        let bridge = Arc::new(Self {
            audio,
            plugin_path,
            gui: Mutex::new(None),
            mirror: GuiMirror::new(),
        });
        Ok((bridge, bridge_thread))
    }

    /// Hand block `seq` to the bridge without waiting. See
    /// [`AudioBridge::submit`]: `param_changes` is swapped for a free buffer
    /// when it goes, and [`Submitted::block`] means the block was accepted,
    /// never that its output is ready.
    pub fn submit(
        &self,
        seq: u64,
        num_samples: usize,
        midi_events: MidiEventVec,
        param_changes: &mut ParameterChanges,
        note_expression: NoteExpressionChanges,
        harmony: HarmonyInputs,
        transport: TransportInfo,
    ) -> Submitted {
        self.audio.submit(
            seq,
            num_samples,
            midi_events,
            param_changes,
            note_expression,
            harmony,
            transport,
        )
    }

    /// Reserve the buffers a block's parameter changes travel in, each with
    /// room for `queues` queues of `points` points. Control thread. See
    /// [`AudioBridge::reserve_param_changes`].
    pub fn reserve_param_changes(&self, queues: usize, points: usize) {
        self.audio.reserve_param_changes(queues, points);
    }

    /// Block `seq`'s MIDI-out, and any earlier block's still queued. See
    /// [`AudioBridge::take_replies`].
    pub fn take_replies(&self, seq: u64, midi_out: &mut MidiEventVec) -> bool {
        self.audio.take_replies(seq, midi_out)
    }

    /// Install a listener for plugin-originated unsolicited events
    /// (latency changes, parameter write-backs). Invoked on the bridge
    /// thread. Pass `None` to clear.
    pub fn set_listener(&self, listener: Option<BridgeListener>) {
        self.audio.set_listener(listener);
    }

    pub fn set_parameter_rt(&self, param_id: ParamId, value: Normalized) -> bool {
        // Cosmetic GUI mirror (keeps the display in sync): queued for the next
        // `editor_idle`, `try_lock` and a bounded ring so the `_rt` contract
        // stays non-blocking and allocation-free. The authoritative delivery is
        // the audio command below; a dropped mirror write self-heals on the next
        // edit.
        self.mirror.push_param(param_id, value);
        // The wire carries `f32`; the domain is enforced up to this point.
        self.audio.set_parameter_rt(param_id, value)
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
        // The GUI mirror is cosmetic and a main-thread call, so the latest mode
        // waits for the next `editor_idle` (`try_lock`, never block); a dropped
        // glow self-heals on the next mode change.
        self.mirror.push_automation(mode);
        self.audio.set_automation_state_rt(mode)
    }

    pub fn set_sample_rate_rt(&self, rate: SampleRate) -> bool {
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

    /// The bridge thread's count of MIDI events it did not send. See
    /// [`AudioBridge::dropped_midi_cell`](super::audio::AudioBridge::dropped_midi_cell).
    pub(crate) fn dropped_midi_cell(&self) -> Arc<std::sync::atomic::AtomicU64> {
        self.audio.dropped_midi_cell()
    }

    pub fn is_crashed(&self) -> bool {
        self.audio.is_crashed()
    }

    /// How many plugin parameter-change events the bridge dropped. See
    /// [`AudioBridge::dropped_parameter_events`](super::audio::AudioBridge::dropped_parameter_events).
    pub fn dropped_parameter_events(&self) -> u64 {
        self.audio.dropped_parameter_events()
    }

    /// How many abandoned replies the bridge thread has taken back off the
    /// socket. See
    /// [`AudioBridge::settled_replies`](super::audio::AudioBridge::settled_replies).
    #[cfg(test)]
    pub fn settled_replies(&self) -> u64 {
        self.audio.settled_replies()
    }

    /// How many blocks' replies the bridge thread abandoned. See
    /// [`AudioBridge::abandoned_replies`](super::audio::AudioBridge::abandoned_replies).
    #[cfg(test)]
    pub fn abandoned_replies(&self) -> u64 {
        self.audio.abandoned_replies()
    }

    /// See [`AudioBridge::block_counts`](super::audio::AudioBridge::block_counts).
    pub fn block_counts(&self) -> crate::host::handles::BlockCounts {
        self.audio.block_counts()
    }

    /// See [`AudioBridge::mark_crashed`](super::audio::AudioBridge::mark_crashed).
    pub fn mark_crashed(&self, cause: &str) {
        self.audio.mark_crashed(cause);
    }

    /// Why the bridge died, or `None` while it is alive. See
    /// [`AudioBridge::crash_cause`](super::audio::AudioBridge::crash_cause).
    pub fn crash_cause(&self) -> Option<String> {
        self.audio.crash_cause()
    }

    /// The refused rate and reason, while the plugin is refused. See
    /// [`AudioBridge::rate_refusal`](super::audio::AudioBridge::rate_refusal).
    pub fn rate_refusal(&self) -> Option<(SampleRate, String)> {
        self.audio.rate_refusal()
    }

    /// See [`AudioBridge::set_rate_timeout`](super::audio::AudioBridge::set_rate_timeout).
    #[cfg(test)]
    pub fn set_rate_timeout(&self, timeout: std::time::Duration) {
        self.audio.set_rate_timeout(timeout);
    }

    pub fn open_editor(
        &self,
        main: &MainThread,
        parent_ptr: *mut c_void,
        scale: ContentScale,
    ) -> std::result::Result<EditorSize, EditorError> {
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
                crate::format::gui::load_gui_instance(main, &self.plugin_path).map_err(|e| {
                    EditorError::PluginError(format!("failed to load in-process GUI: {e}"))
                })?;
            tracing::info!("[bridge] open_editor: in-process GUI loaded successfully");
            *guard = Some(instance);
        }

        let gui = guard.as_mut().expect("just inserted");

        // Sync state from audio (source of truth) to GUI after opening.
        // Some plugins crash if set_state is called before the editor is attached.
        // Best-effort: a state we cannot read means the GUI instance opens at
        // its defaults, which is visibly wrong but not destructive — the audio
        // instance remains the source of truth and nothing is written back. So
        // this logs rather than failing the editor open, which would leave the
        // user with no window at all.
        let state_to_sync = self.audio.save_state().inspect_err(|e| {
            tracing::warn!("[bridge] could not mirror plugin state into the editor: {e}");
        });

        tracing::info!("[bridge] open_editor: calling gui.open_editor(parent={parent_ptr:?})");
        // SAFETY: `parent_ptr` is the platform-native window pointer
        // extracted by `extract_platform_ptr` from a caller-supplied
        // `RawWindowHandle`, which is alive for the open_editor call.
        let size = gui
            .open_editor(main, unsafe { WindowHandle::from_ptr(parent_ptr) }, scale)
            .map_err(|e| EditorError::PluginError(e.to_string()))?;
        tracing::info!(
            "[bridge] open_editor: editor opened {}x{}",
            size.width,
            size.height
        );

        // Sync state after editor is open (some plugins need the view attached first).
        if let Ok(state) = state_to_sync {
            tracing::info!(
                "[bridge] open_editor: syncing {} bytes of state post-open",
                state.len()
            );
            let _ = gui.set_state(main, &state);
        }
        self.mirror.opened();

        Ok(size)
    }

    /// [`open_editor`](Self::open_editor) for a plugin-owned floating window.
    ///
    /// Same lazy GUI load and the same post-open state sync — the editor
    /// instance and its state are the plugin's either way. What differs is only
    /// that no parent goes in and no size comes out.
    pub fn open_floating_editor(&self, main: &MainThread) -> std::result::Result<(), EditorError> {
        if self.audio.is_crashed() {
            return Err(EditorError::PluginCrashed);
        }

        let mut guard = self.gui.lock().map_err(|_| EditorError::Busy)?;
        if guard.is_none() {
            let instance =
                crate::format::gui::load_gui_instance(main, &self.plugin_path).map_err(|e| {
                    EditorError::PluginError(format!("failed to load in-process GUI: {e}"))
                })?;
            *guard = Some(instance);
        }
        let gui = guard.as_mut().expect("just inserted");

        // Read before the open, as the embedded path does: audio is the source
        // of truth for state, and some plugins refuse `set_state` until their
        // view exists.
        // Best-effort: a state we cannot read means the GUI instance opens at
        // its defaults, which is visibly wrong but not destructive — the audio
        // instance remains the source of truth and nothing is written back. So
        // this logs rather than failing the editor open, which would leave the
        // user with no window at all.
        let state_to_sync = self.audio.save_state().inspect_err(|e| {
            tracing::warn!("[bridge] could not mirror plugin state into the editor: {e}");
        });

        gui.open_floating_editor(main)
            .map_err(|e| EditorError::PluginError(e.to_string()))?;

        if let Ok(state) = state_to_sync {
            let _ = gui.set_state(main, &state);
        }
        self.mirror.opened();

        Ok(())
    }

    pub fn close_editor(&self, main: &MainThread) -> bool {
        self.mirror.closed();
        let Ok(mut guard) = self.gui.lock() else {
            return false;
        };
        if let Some(gui) = guard.as_mut() {
            // Flush pending GUI param changes before closing.
            for (param_id, value) in gui.poll_gui_param_changes(main) {
                self.audio
                    .set_parameter_rt(param_id, Normalized::clamped(f64::from(value)));
            }
            gui.close_editor(main);
            true
        } else {
            false
        }
    }

    pub fn editor_idle(&self, main: &MainThread) {
        if let Ok(mut guard) = self.gui.lock()
            && let Some(gui) = guard.as_mut()
        {
            gui.editor_idle(main);
            // Apply what the realtime setters queued for the editor's display.
            if let Ok(mut drain) = self.mirror.drain.lock() {
                drain.drain(|edit| {
                    if let Edit::Param { id, value } = edit {
                        gui.set_parameter(main, id, value);
                    }
                });
            }
            if let Ok(mut latest) = self.mirror.automation.lock()
                && let Some(mode) = latest.take()
            {
                gui.set_automation_state(main, mode);
            }
            // Forward GUI-originated parameter changes to the audio bridge.
            for (param_id, value) in gui.poll_gui_param_changes(main) {
                self.audio
                    .set_parameter_rt(param_id, Normalized::clamped(f64::from(value)));
            }
        }
    }

    pub fn save_state(&self) -> std::result::Result<Vec<u8>, crate::error::StateError> {
        self.audio.save_state()
    }

    /// The **audio** instance's answer is the one returned.
    ///
    /// The GUI instance is a second dlopen of the same plugin kept only so its
    /// editor displays the right values; a failure to mirror there leaves the
    /// editor stale but the audio correct, which is not what a caller asking
    /// "did my preset load" is asking about. Mirroring stays best-effort, and
    /// the audio result is the return value.
    pub fn load_state(
        &self,
        main: &MainThread,
        data: &[u8],
    ) -> std::result::Result<(), StateError> {
        let audio = self.audio.load_state(data);
        // Also load into GUI instance so its display stays in sync.
        if let Ok(mut guard) = self.gui.lock()
            && let Some(gui) = guard.as_mut()
        {
            let _ = gui.set_state(main, data);
        }
        audio
    }

    pub fn parameters(&self) -> Option<Vec<ParameterInfo>> {
        self.audio.parameters()
    }

    pub fn parameter(&self, param_id: ParamId) -> Option<Normalized> {
        self.audio.parameter(param_id)
    }

    pub fn parameter_text(&self, param_id: ParamId, value: Normalized) -> Option<String> {
        self.audio.parameter_text(param_id, value)
    }

    pub fn parameter_value_from_text(&self, param_id: ParamId, text: &str) -> Option<Normalized> {
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

    /// Wait until every command queued so far has been handled and every
    /// event it caused has reached the listener. See the audio bridge's
    /// `settle`.
    pub fn settle(&self) {
        self.audio.settle()
    }

    pub fn editor_capabilities(&self, main: &MainThread) -> EditorCapabilities {
        let Ok(mut guard) = self.gui.lock() else {
            return EditorCapabilities::default();
        };
        match guard.as_mut() {
            Some(gui) => gui.editor_capabilities(main),
            None => EditorCapabilities::default(),
        }
    }

    pub fn set_editor_size(
        &self,
        main: &MainThread,
        requested: EditorSize,
    ) -> std::result::Result<EditorSize, EditorError> {
        let mut guard = self.gui.lock().map_err(|_| EditorError::Busy)?;
        let gui = guard
            .as_mut()
            .ok_or_else(|| EditorError::PluginError("editor not open".into()))?;
        gui.set_editor_size(main, requested)
            .map_err(|e| EditorError::PluginError(e.to_string()))
    }

    pub fn poll_editor_resize_request(&self, main: &MainThread) -> Option<EditorSize> {
        let mut guard = self.gui.lock().ok()?;
        guard.as_mut()?.poll_editor_resize_request(main)
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

    fn parameter_value(&self, id: ParamId) -> Option<Normalized> {
        self.bridge.parameter(id)
    }

    fn parameter_text(&self, id: ParamId, value: Normalized) -> Option<String> {
        self.bridge.parameter_text(id, value)
    }

    /// Asked of the audio instance, like every other parameter read here.
    ///
    /// On VST2 this also *writes* that instance (`effString2Parameter` parses by
    /// applying), which is the same instance `set_parameter_value` targets — so
    /// the value lands where a knob poke would, and the GUI mirror follows
    /// through the existing `PluginParamValuesChanged` refresh rather than a
    /// second write from here.
    fn parameter_value_from_text(&self, id: ParamId, text: &str) -> Option<Normalized> {
        self.bridge.parameter_value_from_text(id, text)
    }

    fn set_parameter_value(&self, id: ParamId, value: Normalized) {
        // Stays `Normalized` all the way to the wire, which narrows it to an
        // `f32`. The subprocess re-clamps on receipt — it must, since the wire
        // is a foreign boundary and the peer is another process — but the value
        // leaving here is already on the unit interval.
        self.bridge.set_parameter_rt(id, value);
    }

    fn is_crashed(&self) -> bool {
        self.bridge.is_crashed()
    }

    fn crash_cause(&self) -> Option<String> {
        self.bridge.crash_cause()
    }

    fn block_counts(&self) -> crate::host::handles::BlockCounts {
        self.bridge.block_counts()
    }

    fn terminate(&self, cause: &str) {
        // The cause first: the kill makes the bridge thread read an EOF, and
        // the first cause latched wins.
        self.bridge.mark_crashed(cause);
        self._guard.terminate();
    }

    fn rate_refusal(&self) -> Option<(SampleRate, String)> {
        self.bridge.rate_refusal()
    }

    fn dropped_parameter_events(&self) -> u64 {
        self.bridge.dropped_parameter_events()
    }
}

impl crate::host::handles::capabilities::HostState for SubprocessBackend {
    fn save_state(&self) -> std::result::Result<Vec<u8>, StateError> {
        self.bridge.save_state()
    }

    fn load_state(&self, main: &MainThread, data: &[u8]) -> std::result::Result<(), StateError> {
        self.bridge.load_state(main, data)
    }
}

impl crate::host::handles::capabilities::HostEditor for SubprocessBackend {
    fn open_editor(
        &self,
        main: &MainThread,
        parent_ptr: *mut c_void,
        scale: ContentScale,
    ) -> std::result::Result<EditorSize, EditorError> {
        self.bridge.open_editor(main, parent_ptr, scale)
    }

    fn open_floating_editor(&self, main: &MainThread) -> std::result::Result<(), EditorError> {
        self.bridge.open_floating_editor(main)
    }

    fn close_editor(&self, main: &MainThread) {
        self.bridge.close_editor(main);
    }

    fn editor_idle(&self, main: &MainThread) {
        self.bridge.editor_idle(main);
    }

    fn editor_capabilities(&self, main: &MainThread) -> EditorCapabilities {
        self.bridge.editor_capabilities(main)
    }

    fn set_editor_size(
        &self,
        main: &MainThread,
        requested: EditorSize,
    ) -> std::result::Result<EditorSize, EditorError> {
        self.bridge.set_editor_size(main, requested)
    }

    fn poll_editor_resize_request(&self, main: &MainThread) -> Option<EditorSize> {
        self.bridge.poll_editor_resize_request(main)
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
    fn presets(&self, _main: &MainThread) -> Vec<Preset> {
        // A crashed subprocess yields `None`; flattened to an empty list
        // because the trait's contract is "what the plugin advertises", and a
        // caller distinguishing "cannot ask" from "listed nothing" reads
        // `Features::PRESET_LIST` rather than a sentinel here.
        self.bridge.presets().unwrap_or_default()
    }

    fn load_preset(&self, _main: &MainThread, id: &PresetId) -> bool {
        !self.bridge.is_crashed() && self.bridge.load_preset(id.clone())
    }

    fn current_preset(&self, _main: &MainThread) -> Option<PresetId> {
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
        // A drop can run on a worker thread (the graph's editor drops a retired
        // node on the thread that collects the commit, which can be off-main),
        // where there is no token. The editor is closed when the drop happens to
        // be on the main thread; off it, the instance is leaked below with its
        // editor still open, which is what an app that closes the editor first
        // (the `NonSend` editor system) never meets.
        if let Some(main) = MainThread::current() {
            self.close_editor(&main);
        }
        if let Ok(mut guard) = self.gui.lock()
            && let Some(gui) = guard.take()
        {
            std::mem::forget(gui);
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

#[cfg(test)]
mod gui_mirror_tests {
    use super::*;

    fn applied(mirror: &GuiMirror) -> Vec<ParamId> {
        let mut out = Vec::new();
        mirror.drain.lock().unwrap().drain(|edit| {
            if let Edit::Param { id, .. } = edit {
                out.push(id);
            }
        });
        out
    }

    /// **An edit made with no editor open is not replayed by the first idle.**
    /// Automation runs with the editor closed; the editor opens; the first idle
    /// applies nothing stale. A write made while it is open is applied; what is
    /// still queued when it closes, or when it opens again, is dropped.
    ///
    /// Mutations (run): `push_param` not checking `open` -> the first assert
    /// fails (a write is queued with no editor); `closed` not discarding -> the
    /// third fails; `opened` not discarding -> the fourth fails.
    #[test]
    fn automation_with_the_editor_closed_is_not_replayed_when_it_opens() {
        let mirror = GuiMirror::new();
        let id = ParamId::new(3);
        mirror.push_param(id, Normalized::of(0.9));
        mirror.push_automation(crate::protocol::AutomationMode::Writing);
        assert!(
            applied(&mirror).is_empty(),
            "nothing is queued with no editor"
        );
        mirror.opened();
        assert!(
            applied(&mirror).is_empty(),
            "the first idle applies nothing stale"
        );

        mirror.push_param(id, Normalized::of(0.2));
        assert_eq!(
            applied(&mirror),
            vec![id],
            "an open editor is shown the write"
        );

        mirror.push_param(id, Normalized::of(0.4));
        mirror.closed();
        assert!(applied(&mirror).is_empty(), "closing drops what waited");

        mirror.opened();
        mirror.push_param(id, Normalized::of(0.6));
        mirror.opened();
        assert!(
            applied(&mirror).is_empty(),
            "opening again drops what waited"
        );
    }
}
