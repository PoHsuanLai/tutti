//! Plugin bridge — composites out-of-process audio with in-process GUI.

use super::audio::{AudioBridge, BridgeListener, BridgeThread, HarmonyInputs, Submitted};
use crate::protocol::{
    MidiEventVec, Normalized, NoteExpressionChanges, ParamId, ParameterChanges, ParameterInfo,
    Preset, PresetId, TransportChanges, TransportInfo,
};
use crate::transport::shm::AudioSlab;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use tutti_plugin_types::Editor;
use tutti_plugin_types::{BridgeResult, Delivered, EditorError, StateError};
use tutti_plugin_types::{ContentScale, EditorCapabilities, EditorSize, WindowHandle};
use tutti_plugin_types::{Edit, EditDrain, EditSink, MainThread, edits};
use tutti_types::SampleRate;

/// Gives the audio instance the editor instance's state when the user changed
/// it: the half of the mirror that carries what the user changed in the editor
/// and is not a parameter (a sample loaded in the plugin's UI, a mode switch;
/// design 021 Q8). Parameters already travel as edits.
///
/// **"Changed" is against `baseline`**, the editor's own state when it was
/// last in step with the audio instance (read right after the open's sync,
/// and moved by each pull). Comparing with the audio instance's state instead
/// took any difference for a user edit: automation or a plugin's volatile
/// state moved the audio side on its own, and the editor's stale state then
/// overwrote it (doc 027). With no baseline (the editor's state could not be
/// read at open) the editor's state is loaded. Nothing is loaded when the
/// editor is at its baseline, so a closed editor that changed nothing does not
/// reload a playing plugin.
///
/// # Errors
///
/// The editor's state cannot be read (nothing is loaded), or the audio
/// instance refuses it (the baseline stays, so the next pull tries again).
fn pull_editor_state(
    main: &MainThread,
    gui: &mut dyn Editor,
    baseline: &mut Option<Vec<u8>>,
    load: impl FnOnce(&[u8]) -> std::result::Result<(), StateError>,
) -> std::result::Result<bool, String> {
    let editor = gui
        .save_state(main)
        .map_err(|e| format!("reading the editor's state: {e}"))?;
    if baseline.as_deref() == Some(editor.as_slice()) {
        return Ok(false);
    }
    load(&editor).map_err(|e| format!("loading it into the audio instance: {e}"))?;
    *baseline = Some(editor);
    Ok(true)
}

/// Composites an out-of-process audio bridge with a lazily-loaded in-process
/// GUI instance. Audio processing goes through `AudioBridge` (IPC to
/// plugin-server child process). Editor/GUI operations load the plugin
/// in-process on first `open_editor` call.
pub struct PluginBridge {
    audio: AudioBridge,
    plugin_path: PathBuf,
    /// Loads the in-process editor instance on the first open: the host's
    /// table of formats, which this crate does not depend on.
    load_editor: EditorLoader,
    gui: Mutex<Option<Box<dyn Editor>>>,
    /// The editor's state when it was last in step with the audio instance:
    /// what [`pull_editor_state`] compares against. Read and written with the
    /// `gui` lock held.
    baseline: Mutex<Option<Vec<u8>>>,
    /// What the realtime setters want the in-process editor to show, applied by
    /// the next [`editor_idle`](Self::editor_idle).
    mirror: GuiMirror,
}

impl std::fmt::Debug for PluginBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginBridge")
            .field("plugin_path", &self.plugin_path)
            .field("audio", &self.audio)
            .finish_non_exhaustive()
    }
}

/// Loads the editor-only instance of the plugin at a path, in this process
/// (`OutOfProcess::load_editor` for the path's format). The host passes its
/// own: which formats it was built with is the host's, not the bridge's.
pub type EditorLoader = fn(&MainThread, &Path) -> BridgeResult<Box<dyn Editor>>;

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
    ///
    /// # Errors
    ///
    /// The bridge thread cannot be spawned.
    pub fn new(
        socket_path: PathBuf,
        audio_buffer: Arc<AudioSlab>,
        plugin_path: PathBuf,
        load_editor: EditorLoader,
        sample_rate: SampleRate,
    ) -> BridgeResult<(Arc<Self>, BridgeThread)> {
        let (audio, bridge_thread) = AudioBridge::new(socket_path, audio_buffer, sample_rate)?;
        let bridge = Arc::new(Self {
            audio,
            plugin_path,
            load_editor,
            gui: Mutex::new(None),
            baseline: Mutex::new(None),
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
        load_editor: EditorLoader,
        sample_rate: SampleRate,
        state_progress_timeout: std::time::Duration,
    ) -> BridgeResult<(Arc<Self>, BridgeThread)> {
        let (mut audio, bridge_thread) = AudioBridge::new(socket_path, audio_buffer, sample_rate)?;
        audio.set_state_progress_timeout(state_progress_timeout);
        let bridge = Arc::new(Self {
            audio,
            plugin_path,
            load_editor,
            gui: Mutex::new(None),
            baseline: Mutex::new(None),
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
        transport_changes: TransportChanges,
    ) -> Submitted {
        self.audio.submit(
            seq,
            num_samples,
            midi_events,
            param_changes,
            note_expression,
            harmony,
            transport,
            transport_changes,
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

    /// [`AudioBridge::set_parameter_rt`], mirrored into an open editor.
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

    /// Sends the host's automation mode to the plugin, mirrored into an open
    /// editor. Realtime-safe; says whether it was delivered.
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

    /// See [`AudioBridge::set_sample_rate_rt`].
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

    /// See [`AudioBridge::reset_rt`].
    pub fn reset_rt(&self) -> bool {
        self.audio.reset_rt()
    }

    /// See [`AudioBridge::audio_buffer`].
    pub fn audio_buffer(&self) -> &Arc<AudioSlab> {
        self.audio.audio_buffer()
    }

    /// The bridge thread's count of MIDI events it did not send. See
    /// `AudioBridge::dropped_midi_cell`.
    pub fn dropped_midi_cell(&self) -> Arc<std::sync::atomic::AtomicU64> {
        self.audio.dropped_midi_cell()
    }

    /// See [`AudioBridge::is_crashed`].
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
    #[cfg(any(test, feature = "test-support"))]
    pub fn settled_replies(&self) -> u64 {
        self.audio.settled_replies()
    }

    /// How many blocks' replies the bridge thread abandoned. See
    /// [`AudioBridge::abandoned_replies`](super::audio::AudioBridge::abandoned_replies).
    #[cfg(any(test, feature = "test-support"))]
    pub fn abandoned_replies(&self) -> u64 {
        self.audio.abandoned_replies()
    }

    /// See [`AudioBridge::block_counts`](super::audio::AudioBridge::block_counts).
    pub fn block_counts(&self) -> tutti_plugin_types::BlockCounts {
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

    /// See [`AudioBridge::rate_refused`](super::audio::AudioBridge::rate_refused).
    pub fn rate_refused(&self) -> bool {
        self.audio.rate_refused()
    }

    /// See [`AudioBridge::set_rate_timeout`](super::audio::AudioBridge::set_rate_timeout).
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_rate_timeout(&self, timeout: std::time::Duration) {
        self.audio.set_rate_timeout(timeout);
    }

    /// The editor-only instance in `slot`, loaded with the [`EditorLoader`] if
    /// this is the first open.
    fn editor_instance<'a>(
        &self,
        main: &MainThread,
        slot: &'a mut Option<Box<dyn Editor>>,
    ) -> std::result::Result<&'a mut Box<dyn Editor>, EditorError> {
        match slot {
            Some(gui) => Ok(gui),
            empty => {
                tracing::info!("[bridge] lazy-loading the in-process GUI");
                let instance = (self.load_editor)(main, &self.plugin_path).map_err(|e| {
                    EditorError::PluginError(format!("failed to load in-process GUI: {e}"))
                })?;
                Ok(empty.insert(instance))
            }
        }
    }

    /// Opens the editor in `parent`, loading the editor-only instance with
    /// the [`EditorLoader`] on the first open and giving it the audio instance's
    /// state.
    ///
    /// # Errors
    ///
    /// The plugin is dead, the editor instance cannot be loaded, or the plugin
    /// refuses to open its editor.
    pub fn open_editor(
        &self,
        main: &MainThread,
        parent: WindowHandle,
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
        let gui = self.editor_instance(main, &mut guard)?;

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

        tracing::info!("[bridge] open_editor: calling gui.open_editor(parent={parent:?})");
        let size = gui
            .open_editor(main, parent, scale)
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
        self.take_baseline(main, gui.as_mut());
        self.mirror.opened();

        Ok(size)
    }

    /// [`open_editor`](Self::open_editor) for a plugin-owned floating window.
    ///
    /// Same lazy GUI load and the same post-open state sync — the editor
    /// instance and its state are the plugin's either way. What differs is only
    /// that no parent goes in and no size comes out.
    ///
    /// # Errors
    ///
    /// As [`open_editor`](Self::open_editor).
    pub fn open_floating_editor(&self, main: &MainThread) -> std::result::Result<(), EditorError> {
        if self.audio.is_crashed() {
            return Err(EditorError::PluginCrashed);
        }

        let mut guard = self.gui.lock().map_err(|_| EditorError::Busy)?;
        let gui = self.editor_instance(main, &mut guard)?;

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
        self.take_baseline(main, gui.as_mut());
        self.mirror.opened();

        Ok(())
    }

    /// Closes the editor, after sending the audio instance what the user changed
    /// in it. `false` when no editor was open.
    pub fn close_editor(&self, main: &MainThread) -> bool {
        self.mirror.closed();
        let Ok(mut guard) = self.gui.lock() else {
            return false;
        };
        if let Some(gui) = guard.as_mut() {
            // Flush pending GUI param changes before closing.
            for (param_id, value) in gui.poll_edits(main) {
                self.audio
                    .set_parameter_rt(param_id, Normalized::clamped(f64::from(value)));
            }
            // What the user changed in the editor that is not a parameter
            // reaches the audio instance (design 021 Q8). Read while the view
            // is still attached.
            self.pull_editor_state(main, gui.as_mut());
            gui.close_editor(main);
            *self.baseline.lock().unwrap_or_else(PoisonError::into_inner) = None;
            true
        } else {
            false
        }
    }

    /// The editor's main-thread tick: applies what the realtime setters queued
    /// for it and sends the audio instance the user's edits.
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
            for (param_id, value) in gui.poll_edits(main) {
                self.audio
                    .set_parameter_rt(param_id, Normalized::clamped(f64::from(value)));
            }
            // The plugin said its state changed: give the audio instance the
            // editor's state now rather than at close.
            if gui.take_dirty(main) {
                self.pull_editor_state(main, gui.as_mut());
            }
        }
    }

    /// The editor's state as it stands now, as the baseline for the next
    /// [`pull_editor_state`]: called once the open has synced it from the
    /// audio instance. Read back rather than taken from the sync's bytes,
    /// because a plugin may normalise a state it is given.
    fn take_baseline(&self, main: &MainThread, gui: &mut dyn Editor) {
        *self.baseline.lock().unwrap_or_else(PoisonError::into_inner) = gui.save_state(main).ok();
    }

    /// [`pull_editor_state`] against this bridge's audio instance.
    fn pull_editor_state(&self, main: &MainThread, gui: &mut dyn Editor) {
        let mut baseline = self.baseline.lock().unwrap_or_else(PoisonError::into_inner);
        let pulled = pull_editor_state(main, gui, &mut baseline, |state| {
            self.audio.load_state(state)
        });
        if let Err(e) = pulled {
            tracing::warn!("[bridge] the editor's state did not reach the audio instance: {e}");
        }
    }

    /// See [`AudioBridge::save_state`].
    ///
    /// # Errors
    ///
    /// As [`AudioBridge::save_state`].
    pub fn save_state(&self) -> std::result::Result<Vec<u8>, tutti_plugin_types::StateError> {
        self.audio.save_state()
    }

    /// The **audio** instance's answer is the one returned.
    ///
    /// The GUI instance is a second dlopen of the same plugin kept only so its
    /// editor displays the right values; a failure to mirror there leaves the
    /// editor stale but the audio correct, which is not what a caller asking
    /// "did my preset load" is asking about. Mirroring stays best-effort, and
    /// the audio result is the return value.
    ///
    /// # Errors
    ///
    /// As [`AudioBridge::load_state`].
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

    /// See [`AudioBridge::parameters`].
    pub fn parameters(&self) -> Option<Vec<ParameterInfo>> {
        self.audio.parameters()
    }

    /// See [`AudioBridge::parameter`].
    pub fn parameter(&self, param_id: ParamId) -> Option<Normalized> {
        self.audio.parameter(param_id)
    }

    /// The plugin's text for a parameter value, or `None` when it cannot be asked.
    pub fn parameter_text(&self, param_id: ParamId, value: Normalized) -> Option<String> {
        self.audio.parameter_text(param_id, value)
    }

    /// The value the plugin parses `text` to, or `None` when it cannot be asked
    /// or does not parse it.
    pub fn parameter_value_from_text(&self, param_id: ParamId, text: &str) -> Option<Normalized> {
        self.audio.parameter_value_from_text(param_id, text)
    }

    /// The plugin's presets, or `None` when it cannot be asked.
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

    /// The preset the plugin says is loaded, if it says.
    pub fn current_preset(&self) -> Option<PresetId> {
        self.audio.current_preset()
    }

    /// Wait until every command queued so far has been handled and every
    /// event it caused has reached the listener. See the audio bridge's
    /// `settle`.
    pub fn settle(&self) {
        self.audio.settle()
    }

    /// The open editor's resize capabilities (the default when none is open).
    pub fn editor_capabilities(&self, main: &MainThread) -> EditorCapabilities {
        let Ok(mut guard) = self.gui.lock() else {
            return EditorCapabilities::default();
        };
        match guard.as_mut() {
            Some(gui) => gui.editor_capabilities(main),
            None => EditorCapabilities::default(),
        }
    }

    /// Asks the open editor to take `requested`; answers with the size it took.
    ///
    /// # Errors
    ///
    /// No editor is open, or it cannot resize.
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

    /// A size the open editor asked its window to take since the last poll.
    pub fn poll_editor_resize_request(&self, main: &MainThread) -> Option<EditorSize> {
        let mut guard = self.gui.lock().ok()?;
        guard.as_mut()?.poll_editor_resize_request(main)
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

#[cfg(test)]
mod state_back_tests {
    //! The editor's state reaching the audio instance (design 021 Q8), on a
    //! stand-in editor: the decision is the bridge's, and each format's
    //! `save_state` is its host crate's.
    use super::*;
    use std::cell::RefCell;
    use tutti_fixtures::main_thread;
    use tutti_plugin_types::{EditorWindow, PluginError, PluginResult};

    struct FakeEditor {
        /// The editor's state, or `None` when it cannot be read.
        state: Option<Vec<u8>>,
    }

    impl Editor for FakeEditor {
        fn set_parameter(&mut self, _: &MainThread, _: ParamId, _: Normalized) {}
        fn set_state(&mut self, _: &MainThread, _: &[u8]) -> PluginResult<()> {
            Ok(())
        }
        fn poll_edits(&mut self, _: &MainThread) -> Vec<(ParamId, f32)> {
            Vec::new()
        }
        fn save_state(&mut self, _: &MainThread) -> PluginResult<Vec<u8>> {
            self.state
                .clone()
                .ok_or_else(|| PluginError::State("unreadable".into()))
        }
    }

    impl EditorWindow for FakeEditor {
        fn has_editor(&self, _: &MainThread) -> bool {
            false
        }

        fn open_editor(
            &mut self,
            _: &MainThread,
            _: WindowHandle,
            _: ContentScale,
        ) -> PluginResult<EditorSize> {
            unreachable!("the state-back never opens")
        }

        fn close_editor(&mut self, _: &MainThread) {}

        fn editor_idle(&mut self, _: &MainThread) {}
    }

    /// What the audio instance was given, if anything, and the baseline after.
    fn pull(
        editor: Option<Vec<u8>>,
        baseline: Option<Vec<u8>>,
    ) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
        let loaded = RefCell::new(None);
        let mut gui = FakeEditor { state: editor };
        let mut baseline = baseline;
        let _ = pull_editor_state(&main_thread(), &mut gui, &mut baseline, |state| {
            *loaded.borrow_mut() = Some(state.to_vec());
            Ok(())
        });
        (loaded.into_inner(), baseline)
    }

    /// **A state the user changed in the editor reaches the audio instance.**
    /// (Was "a state only the editor holds", against the audio instance's
    /// state; the comparison is against the baseline now.)
    ///
    /// Mutation (run): return before `load` in `pull_editor_state` → nothing is
    /// loaded → fails.
    #[test]
    fn a_state_only_the_editor_holds_reaches_the_audio_instance() {
        let (got, _) = pull(Some(b"mode=delay".to_vec()), Some(b"mode=gain".to_vec()));
        assert_eq!(got.as_deref(), Some(&b"mode=delay"[..]));
    }

    /// **An editor that changed nothing does not reload the audio instance**,
    /// whatever the audio instance did meanwhile: its state is not consulted,
    /// so automation that moved it is not overwritten by the editor's stale
    /// one (doc 027).
    ///
    /// Mutation (run): drop the equality check → the same state is loaded →
    /// fails.
    #[test]
    fn an_unchanged_editor_reloads_nothing() {
        assert_eq!(pull(Some(b"same".to_vec()), Some(b"same".to_vec())).0, None);
    }

    /// **With no baseline the editor's state is taken.** No baseline is not
    /// evidence the editor changed nothing. (Was "an unreadable audio state".)
    ///
    /// Mutation (run): treat `None` as equal (`baseline.as_deref().map_or(true,
    /// ..)`) → nothing is loaded → fails.
    #[test]
    fn an_unreadable_audio_state_still_takes_the_editors() {
        let (got, _) = pull(Some(b"editor".to_vec()), None);
        assert_eq!(got.as_deref(), Some(&b"editor"[..]));
    }

    /// **An editor whose state cannot be read gives the audio instance
    /// nothing**, not an empty state.
    ///
    /// Mutation (run): `unwrap_or_default()` on the editor's state → an empty
    /// state is loaded → fails.
    #[test]
    fn an_unreadable_editor_state_loads_nothing() {
        let (got, _) = pull(None, Some(b"audio".to_vec()));
        assert_eq!(got, None);
    }

    /// **A pull moves the baseline**, so the same edit is not loaded again at
    /// the next dirty tick or at close.
    ///
    /// Mutation (run): drop `*baseline = Some(editor)` → the second pull loads
    /// again → fails.
    #[test]
    fn a_pulled_state_is_the_new_baseline() {
        let (got, baseline) = pull(Some(b"edited".to_vec()), Some(b"opened".to_vec()));
        assert!(got.is_some());
        assert_eq!(pull(Some(b"edited".to_vec()), baseline).0, None);
    }
}
