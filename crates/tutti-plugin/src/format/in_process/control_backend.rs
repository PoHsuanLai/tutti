//! Host-side capability backend for an in-process plugin of any [`InProcess`]
//! format.
//!
//! Implements [`HostParams`], [`HostState`], [`HostEditor`], [`HostPresets`](crate::backend::HostPresets) and
//! [`HostRenderMode`] over the plugin's [`Control`](tutti_plugin_types::Control) surface
//! ([`Lifecycle::control`](tutti_plugin_types::Lifecycle::control), which
//! reaches it whether the plugin is loaded or active). Holds the plugin's
//! control side, the `Arc<Mutex<Instance<F>>>` the node's `shape` and
//! `prepare` also take. Control-thread calls take the lock for the duration of
//! one plugin operation — short for parameter / state methods, potentially long
//! for editor ones. The audio thread never takes it: it renders through the
//! Processor lent to the node, so a slow `editor_idle` cannot underrun audio.
//!
//! # Threads
//!
//! Every [`Control`](tutti_plugin_types::Control) entry is a main-thread call
//! that takes a [`MainThread`]. The `Host*` traits take one where the call is
//! the caller's to place ([`HostState::load_state`], the presets, the editor);
//! the rest ([`HostParams`], [`HostState::save_state`], [`HostRenderMode`])
//! take none, because the out-of-process backends answer them from any thread,
//! and the session calls them from pool jobs. Here they work from any thread
//! too, through `on_main`: on the main thread the call runs at once; off it the
//! work is posted with [`defer_to_main`] and the caller waits for the answer,
//! which costs one main-thread tick (`Session::update` runs
//! [`run_deferred`](tutti_plugin_types::run_deferred) every tick). The wait has
//! a deadline ([`OFF_MAIN_DEADLINE`]): the host's main thread must keep
//! pumping, and a host that blocks its main thread on such a call (joining the
//! job from the main thread) gets the trait's "unavailable" answer (`None`,
//! `Err`, `false`) when it expires. A timed-out call's work still runs at the
//! next tick. Fire-and-forget writes post and do not wait.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::mpsc::sync_channel;
use std::time::Duration;
use tutti_plugin_types::MainThread;

use parking_lot::Mutex;

use super::instance::Instance;
use crate::host::handles::capabilities::{HostEditor, HostParams, HostRenderMode, HostState};
use crate::host::node::ParameterChangeSink;
use crate::util::window::{ContentScale, EditorSize, WindowHandle};
use tutti_plugin_types::{EditorError, InProcess, PluginError, StateError, defer_to_main};
use tutti_plugin_types::{Normalized, ParamId, ParameterInfo, RenderMode};
use tutti_plugin_types::{Preset, PresetId};

/// Bundles the shared Mutex with the parameter-change sink so editor
/// idle calls can drain plugin-internal automation events and notify
/// the user-installed callback.
pub(crate) struct ControlBackend<F: InProcess> {
    pub(super) inner: Arc<Mutex<Instance<F>>>,
    pub(crate) param_sink: ParameterChangeSink,
    /// The cell the audio unit parks a rate change in, shared with the
    /// node. Drained here because telling a plugin its rate deactivates and
    /// activates it, which allocates.
    pub(crate) pending_sample_rate: Arc<AtomicU64>,
    /// Where a re-read that found the plugin's latency, tail or pins moved is
    /// reported (`PluginHandle::on_invalidate`).
    pub(crate) invalidate_sink: crate::host::node::InvalidateSink,
}

/// How long an off-main-thread call waits for the main thread to run its work
/// (see the module docs, "Threads"). Under `cfg(test)` it is shortened to one
/// second so the test of the expiry does not stall the suite; the pumped tests
/// answer in one tick.
const OFF_MAIN_DEADLINE: Duration = if cfg!(test) {
    Duration::from_secs(1)
} else {
    Duration::from_secs(2)
};

impl<F> ControlBackend<F>
where
    F: InProcess + 'static,
    F::Processor: 'static,
{
    /// Runs `work` on the plugin on the main thread, under the instance lock,
    /// from whichever thread asks. `None` when it cannot be answered: no main
    /// thread has been claimed, or the main thread did not run the work within
    /// [`OFF_MAIN_DEADLINE`].
    fn on_main<R: Send + 'static>(
        &self,
        work: impl FnOnce(&MainThread, &mut Instance<F>) -> R + Send + 'static,
    ) -> Option<R> {
        if let Some(main) = MainThread::current() {
            return Some(work(&main, &mut self.inner.lock()));
        }
        if !MainThread::claimed_elsewhere() {
            return None;
        }
        let (answer, wait) = sync_channel(1);
        let inner = Arc::clone(&self.inner);
        defer_to_main(move |main| {
            let _ = answer.send(work(main, &mut inner.lock()));
        });
        wait.recv_timeout(OFF_MAIN_DEADLINE).ok()
    }

    /// `work` on the main thread without waiting: at once on it, posted off it
    /// (and dropped when no main thread exists). Skipped when the instance is
    /// locked, as a write has always been.
    fn post(&self, work: impl FnOnce(&MainThread, &mut Instance<F>) + Send + 'static) {
        if let Some(main) = MainThread::current() {
            if let Some(mut instance) = self.inner.try_lock() {
                work(&main, &mut instance);
            }
        } else if MainThread::claimed_elsewhere() {
            let inner = Arc::clone(&self.inner);
            defer_to_main(move |main| {
                if let Some(mut instance) = inner.try_lock() {
                    work(main, &mut instance);
                }
            });
        }
    }
}

/// A plugin's error as the editor's.
fn editor_error(error: PluginError) -> EditorError {
    match error {
        PluginError::Editor(editor) => editor,
        other => EditorError::PluginError(other.to_string()),
    }
}

impl<F> HostParams for ControlBackend<F>
where
    F: InProcess + 'static,
    F::Processor: 'static,
{
    /// `None` for a lost plugin, and when the main thread does not answer.
    fn parameter_descriptors(&self) -> Option<Vec<ParameterInfo>> {
        self.on_main(|main, instance| Some(instance.control()?.parameters(main)))?
    }

    /// `None` for a lost plugin, for a plugin that exposes no value to read,
    /// for an id it does not have, and when the main thread does not answer.
    fn parameter_value(&self, id: ParamId) -> Option<Normalized> {
        self.on_main(move |main, instance| instance.control()?.parameter(main, id))?
    }

    /// Fire-and-forget, and skipped while the instance is locked (a long
    /// `editor_idle`): lost writes are recoverable, the GUI thread will retry on
    /// the next idle. Off the main thread it is posted to it.
    ///
    /// In-process, so this write reaches the plugin directly with no
    /// subprocess boundary to re-clamp at: it is a `Normalized` rather than a
    /// bare float precisely because nothing downstream would catch one that
    /// was not.
    fn set_parameter_value(&self, id: ParamId, value: Normalized) {
        self.post(move |main, instance| {
            if let Some(control) = instance.control() {
                control.set_parameter(main, id, value);
            }
        });
    }

    /// The plugin's display string for `value`, as
    /// [`Control::parameter_text`](tutti_plugin_types::Control::parameter_text)
    /// answers it (VST2's only for the value it currently holds).
    fn parameter_text(&self, id: ParamId, value: Normalized) -> Option<String> {
        self.on_main(move |main, instance| instance.control()?.parameter_text(main, id, value))?
    }

    /// Parse through the plugin's own parser
    /// ([`Control::parameter_value_from_text`](tutti_plugin_types::Control::parameter_value_from_text)).
    /// **On VST2 this writes**, since its opcode parses by applying.
    fn parameter_value_from_text(&self, id: ParamId, text: &str) -> Option<Normalized> {
        let text = text.to_owned();
        self.on_main(move |main, instance| {
            instance
                .control()?
                .parameter_value_from_text(main, id, &text)
        })?
    }

    fn is_crashed(&self) -> bool {
        // In-process: if the plugin crashed it took the host with it,
        // so a returning caller can never observe a crashed state.
        false
    }

    /// The edits the plugin reported that its bounded queue refused
    /// ([`Control::dropped_reported_edits`](tutti_plugin_types::Control::dropped_reported_edits)).
    /// A blocking `lock`, like the other reads: a count that read `0` on
    /// contention would go backwards. Any thread: a count, not a plugin call.
    fn dropped_parameter_events(&self) -> u64 {
        self.inner
            .lock()
            .control()
            .map_or(0, |control| control.dropped_reported_edits())
    }
}

impl<F> HostState for ControlBackend<F>
where
    F: InProcess + 'static,
    F::Processor: 'static,
{
    /// The plugin's own failure, forwarded rather than dropped, so the caller
    /// can tell "the plugin has no state" from "asking it failed". The
    /// in-process path cannot hit the transport's size limit, so `Rejected` is
    /// the only failure the plugin can produce.
    ///
    /// `effGetChunk` and its kin are main-thread calls, and the health snapshot
    /// runs this on a task pool (`PluginSnapshot::run`): off the main thread it
    /// is run by the main thread's next tick (`on_main`), and
    /// [`StateError::Unavailable`] if that does not come within the deadline.
    fn save_state(&self) -> Result<Vec<u8>, StateError> {
        match self.on_main(|main, instance| {
            instance
                .control()
                .map(|control| control.save_state(main).map_err(|e| e.to_string()))
        }) {
            Some(Some(state)) => state.map_err(StateError::Rejected),
            Some(None) => Err(StateError::Rejected("the plugin is lost".into())),
            None => Err(StateError::Unavailable),
        }
    }

    /// The plugin's own refusal, forwarded rather than dropped.
    fn load_state(&self, main: &MainThread, data: &[u8]) -> Result<(), StateError> {
        match self.inner.lock().control() {
            Some(control) => control
                .load_state(main, data)
                .map_err(|e| StateError::Rejected(e.to_string())),
            None => Err(StateError::Rejected("the plugin is lost".into())),
        }
    }
}

impl<F> crate::backend::HostPresets for ControlBackend<F>
where
    F: InProcess + 'static,
    F::Processor: 'static,
{
    /// The plugin's presets, in its own order ([`Control::presets`](tutti_plugin_types::Control::presets)).
    fn presets(&self, main: &MainThread) -> Vec<Preset> {
        self.inner
            .lock()
            .control()
            .map(|control| control.presets(main))
            .unwrap_or_default()
    }

    /// `false` for an id this format cannot address, and for a lost plugin.
    fn load_preset(&self, main: &MainThread, id: &PresetId) -> bool {
        self.inner
            .lock()
            .control()
            .is_some_and(|control| control.load_preset(main, id))
    }

    fn current_preset(&self, main: &MainThread) -> Option<PresetId> {
        self.inner.lock().control()?.current_preset(main)
    }
}

impl<F> HostRenderMode for ControlBackend<F>
where
    F: InProcess + 'static,
    F::Processor: 'static,
{
    /// Whether the plugin runs in `mode` now: told live where its format takes
    /// it (VST2 always does: the host answers
    /// `audioMasterGetCurrentProcessLevel` whenever the plugin asks), else
    /// reconfigured through the lifecycle so the next activation carries it
    /// (`Instance::set_render_mode`).
    ///
    /// Off the main thread it runs on the main thread's next tick, and is
    /// `false` if that does not come within the deadline.
    /// `InProcClient::set_render_mode` is the same call on the node's side.
    fn set_render_mode(&self, mode: RenderMode) -> bool {
        self.on_main(move |main, instance| instance.set_render_mode(main, mode))
            .unwrap_or(false)
    }
}

impl<F> HostEditor for ControlBackend<F>
where
    F: InProcess + 'static,
    F::Processor: 'static,
{
    fn open_editor(
        &self,
        main: &MainThread,
        parent_ptr: *mut c_void,
        scale: ContentScale,
    ) -> std::result::Result<EditorSize, EditorError> {
        // SAFETY: `parent_ptr` is the platform-native window pointer
        // (NSView*, HWND, X11 window id) extracted from a caller-supplied
        // `RawWindowHandle`, alive for the call; the plugin's format only
        // forwards it to the plugin's own embed call.
        let parent = unsafe { WindowHandle::from_ptr(parent_ptr) };
        let mut instance = self.inner.lock();
        let control = instance
            .control()
            .ok_or_else(|| EditorError::PluginError("the plugin is lost".into()))?;
        control
            .open_editor(main, parent, scale)
            .map_err(editor_error)
    }

    fn open_floating_editor(&self, main: &MainThread) -> std::result::Result<(), EditorError> {
        let mut instance = self.inner.lock();
        let control = instance
            .control()
            .ok_or_else(|| EditorError::PluginError("the plugin is lost".into()))?;
        control.open_floating_editor(main).map_err(editor_error)
    }

    fn close_editor(&self, main: &MainThread) {
        if let Some(control) = self.inner.lock().control() {
            control.close_editor(main);
        }
    }

    fn editor_idle(&self, main: &MainThread) {
        // Before the lock below, not inside it: the drain takes the same lock
        // and would deadlock under that guard. This runs on the main thread,
        // every editor frame, which is what makes it the delivery point for the
        // rate the audio thread had to park.
        super::client::drain_sample_rate(main, &self.pending_sample_rate, &self.inner);

        let (changes, param_changes) = {
            let mut instance = self.inner.lock();
            // What the plugin changed about itself since the last pump: the
            // figures an activation read (a rate change that finished here, once
            // the audio thread handed its Processor back), and what the plugin
            // asked for. Re-read here, on the main thread, where a latency or
            // tail query belongs.
            let changes = instance.pump(main);
            // Before the lost check: a refusal that lost the plugin is the one to report.
            if let Some(report) = instance.take_rate_report() {
                use tutti_plugin_types::PluginInvalidation;
                self.invalidate_sink.fire(match report {
                    Some((rate, reason)) => PluginInvalidation::RateRefused { rate, reason },
                    None => PluginInvalidation::RateRestored,
                });
            }
            let Some(control) = instance.control() else {
                return;
            };
            control.editor_idle(main);
            (changes, instance.drain_param_changes(main))
        };
        // Drain any plugin-internal parameter changes (knob movement on
        // the editor surface) and forward them to the user sink. Fires
        // on the GUI thread — same threading contract as the parameter
        // sink callback for the subprocess backend (which fires on the
        // bridge thread). After the lock is released: a callback may call back.
        for edit in param_changes {
            self.param_sink.fire(edit);
        }
        use tutti_plugin_types::PluginInvalidation;
        if let Some(samples) = changes.latency {
            self.invalidate_sink
                .fire(PluginInvalidation::Latency { samples });
        }
        if let Some(tail) = changes.tail {
            self.invalidate_sink.fire(PluginInvalidation::Tail { tail });
        }
        if changes.io {
            self.invalidate_sink.fire(PluginInvalidation::Io);
        }
    }
}

/// The off-main-thread routing of the handle's methods (`on_main`), against the
/// reference VST2 plugin. `OFF_MAIN_DEADLINE` is one second under `cfg(test)`
/// (not injectable: the backend sits behind the handle's `Arc<dyn ..>`).
#[cfg(all(test, feature = "vst2"))]
mod off_main_tests {
    use tutti_fixtures::main_thread;
    use tutti_plugin_types::{Load, Normalized, ParamId, StateError, run_deferred};
    use tutti_types::SampleRate;
    use tutti_vst2_host::Vst2Loaded;

    use crate::handles::PluginHandle;

    /// The probe, loaded in process, with parameter 1 written to 0.75.
    fn loaded() -> (crate::InProcClient<Vst2Loaded>, PluginHandle) {
        let (client, handle) = crate::load_client::<Vst2Loaded>(
            &main_thread(),
            tutti_fixtures::vst2_probe(),
            &Load::new(),
            SampleRate(48_000.0),
        )
        .unwrap_or_else(|e| panic!("in-process load of the probe: {e:?}"));
        handle
            .params()
            .set_parameter_value(ParamId::new(1), Normalized::of(0.75));
        (client, handle)
    }

    /// **A pool thread's call on the handle is answered by the main thread's
    /// tick**, with the real values: the state equals one saved on the main
    /// thread, and the parameter reads what was written. This is what the
    /// session's snapshot and parameter-mirror jobs do.
    ///
    /// Mutation (run): `on_main` answering `None` off the main thread -> the
    /// state is `Unavailable` and the value `None` -> fails. Mutation (run): the
    /// test not calling `run_deferred` -> the calls wait out the deadline and
    /// answer "unavailable" -> fails.
    #[test]
    fn a_pool_threads_calls_are_answered_by_the_main_threads_tick() {
        let main = main_thread();
        let (_client, handle) = loaded();
        let expected = handle
            .state()
            .save_state()
            .expect("saved on the main thread");
        let id = ParamId::new(1);
        let job = {
            let handle = handle.clone();
            std::thread::spawn(move || {
                (
                    handle.state().save_state(),
                    handle.params().parameter_value(id),
                )
            })
        };
        while !job.is_finished() {
            run_deferred(&main);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let (state, value) = job.join().expect("the job returns");
        assert_eq!(state.expect("saved off the main thread"), expected);
        assert_eq!(value.map(Normalized::get), Some(0.75));
    }

    /// **With nothing pumping the main thread, an off-main call answers
    /// "unavailable" after the deadline**, not never.
    ///
    /// Mutation (run): `recv_timeout` replaced by `recv` -> the thread never
    /// returns -> the test hangs.
    #[test]
    fn an_unpumped_main_thread_gets_the_unavailable_answer() {
        let _main = main_thread();
        let (_client, handle) = loaded();
        let id = ParamId::new(1);
        let (state, value) = std::thread::spawn(move || {
            (
                handle.state().save_state(),
                handle.params().parameter_value(id),
            )
        })
        .join()
        .expect("the job returns");
        assert!(matches!(state, Err(StateError::Unavailable)), "{state:?}");
        assert_eq!(value, None);
    }
}
