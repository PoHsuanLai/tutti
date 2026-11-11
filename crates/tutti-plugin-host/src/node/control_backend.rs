//! An in-process plugin of any [`InProcess`] format, behind a handle: its
//! [`AnyThread`] (the hop to the main thread), its [`Health`] and the
//! [`MainThreadControl`] a handle's token-taking calls reach it by.
//!
//! All three are over the plugin's [`Control`]
//! surface ([`Lifecycle::control`](tutti_plugin_types::Lifecycle::control),
//! which reaches it whether the plugin is loaded or active). Holds the plugin's
//! control side, the `Arc<Mutex<Instance<F>>>` the node's `shape` and
//! `prepare` also take. Control-thread calls take the lock for the duration of
//! one plugin operation — short for parameter / state methods, potentially long
//! for editor ones. The audio thread never takes it: it renders through the
//! Processor lent to the node, so a slow `editor_idle` cannot underrun audio.
//!
//! # Threads
//!
//! Every [`Control`] entry is a main-thread call
//! that takes a [`MainThread`]. [`MainThreadControl`] is lent to a caller that
//! holds one; [`AnyThread`] takes none, because a served plugin answers those
//! calls from any thread and the session calls them from pool jobs. Here they
//! work from any thread too, through `on_main`: on the main thread the call runs
//! at once; off it the
//! work is posted with [`defer_to_main`] and the caller waits for the answer,
//! which costs one main-thread tick (`Session::update` runs
//! [`run_deferred`](tutti_plugin_types::run_deferred) every tick). The wait has
//! a deadline ([`OFF_MAIN_DEADLINE`]): the host's main thread must keep
//! pumping, and a host that blocks its main thread on such a call (joining the
//! job from the main thread) gets the trait's "unavailable" answer (`None`,
//! `Err`, `false`) when it expires. A timed-out call's work still runs at the
//! next tick. Fire-and-forget writes post and do not wait.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::mpsc::sync_channel;
use std::time::Duration;
use tutti_plugin_types::MainThread;

use parking_lot::Mutex;

use super::instance::Instance;
use crate::MainThreadControl;
use crate::ParameterChangeSink;
use tutti_plugin_types::{
    AnyThread, AutomationMode, Control, EditorError, Health, InProcess, StateError, defer_to_main,
};
use tutti_plugin_types::{Normalized, ParamId, ParameterInfo, RenderMode};

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
    pub(crate) invalidate_sink: crate::InvalidateSink,
    /// The plugin's format name, for the refusal of an advisory no in-process
    /// format carries.
    pub(crate) format: &'static str,
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

impl<F> AnyThread for ControlBackend<F>
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
    /// [`Control::parameter_text`]
    /// answers it (VST2's only for the value it currently holds).
    fn parameter_text(&self, id: ParamId, value: Normalized) -> Option<String> {
        self.on_main(move |main, instance| instance.control()?.parameter_text(main, id, value))?
    }

    /// Parse through the plugin's own parser
    /// ([`Control::parameter_value_from_text`]).
    /// **On VST2 this writes**, since its opcode parses by applying.
    fn parameter_value_from_text(&self, id: ParamId, text: &str) -> Option<Normalized> {
        let text = text.to_owned();
        self.on_main(move |main, instance| {
            instance
                .control()?
                .parameter_value_from_text(main, id, &text)
        })?
    }

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

    /// No in-process format carries the VST3-style advisory.
    fn set_automation_mode(&self, _mode: AutomationMode) -> Result<(), EditorError> {
        Err(EditorError::GuiNotSupported {
            format: self.format.to_string(),
        })
    }
}

/// An in-process plugin cannot be observed to die: a crash takes the host with
/// it, so a returning caller never sees a crashed state, and the rest is the
/// trait's defaults.
impl<F: InProcess> Health for ControlBackend<F> {
    fn is_crashed(&self) -> bool {
        false
    }

    fn crash_cause(&self) -> Option<String> {
        None
    }

    fn rate_refusal(&self) -> Option<(tutti_types::SampleRate, String)> {
        None
    }

    fn block_counts(&self) -> tutti_plugin_types::BlockCounts {
        tutti_plugin_types::BlockCounts::default()
    }

    fn terminate(&self, _cause: &str) {}
}

impl<F> MainThreadControl for ControlBackend<F>
where
    F: InProcess + 'static,
    F::Processor: 'static,
{
    /// Under the instance's lock; not called for a lost plugin.
    fn lend(&self, f: &mut dyn FnMut(&mut dyn Control)) {
        if let Some(control) = self.inner.lock().control() {
            f(control);
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
#[cfg(test)]
mod off_main_tests {
    use tutti_fixtures::main_thread;
    use tutti_plugin_types::{
        AutomationMode, EditorError, Load, Normalized, ParamId, RenderMode, StateError,
        run_deferred,
    };
    use tutti_types::SampleRate;
    use tutti_vst2_host::Vst2Loaded;

    use crate::PluginHandle;

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
            .any_thread()
            .set_parameter_value(ParamId::new(1), Normalized::of(0.75));
        (client, handle)
    }

    /// **A pool thread's call on the handle is answered by the main thread's
    /// tick**, with the real values: the state equals one saved on the main
    /// thread, and the parameter reads what was written. This is what the
    /// session's snapshot and parameter-mirror jobs do.
    #[test]
    fn a_pool_threads_calls_are_answered_by_the_main_threads_tick() {
        let main = main_thread();
        let (_client, handle) = loaded();
        let expected = handle
            .any_thread()
            .save_state()
            .expect("saved on the main thread");
        let id = ParamId::new(1);
        let job = {
            let handle = handle.clone();
            std::thread::spawn(move || {
                (
                    handle.any_thread().save_state(),
                    handle.any_thread().parameter_value(id),
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
    #[test]
    fn an_unpumped_main_thread_gets_the_unavailable_answer() {
        let _main = main_thread();
        let (_client, handle) = loaded();
        let id = ParamId::new(1);
        let (state, value) = std::thread::spawn(move || {
            (
                handle.any_thread().save_state(),
                handle.any_thread().parameter_value(id),
            )
        })
        .join()
        .expect("the job returns");
        assert!(matches!(state, Err(StateError::Unavailable)), "{state:?}");
        assert_eq!(value, None);
    }

    /// **The parameter reads answer on the main thread**: the catalog lists
    /// the probe's parameters, the value is the one written, and the text
    /// and parse are the plugin's own (a text it cannot parse is `None`, never a
    /// default that looks like an answer).
    #[test]
    fn the_parameter_reads_answer_on_the_main_thread() {
        let _main = main_thread();
        let (_client, handle) = loaded();
        let any = handle.any_thread();
        let catalog = any.parameter_descriptors().expect("the probe lists");
        assert!(catalog.iter().any(|p| p.id == ParamId::new(1)));
        assert_eq!(
            any.parameter_value(ParamId::new(1)).map(Normalized::get),
            Some(0.75)
        );
        let text = any.parameter_text(ParamId::new(1), Normalized::of(0.75));
        assert!(
            text.as_deref().is_some_and(|t| t.contains("0.75")),
            "the plugin's own display string: {text:?}"
        );
        assert_eq!(
            any.parameter_value_from_text(ParamId::new(1), "no such value"),
            None,
            "a text the plugin cannot parse leaves the field where it was"
        );
    }

    /// **A parameter write lands on the plugin** (on the main thread it runs at
    /// once, under the instance's lock).
    #[test]
    fn a_parameter_write_lands_on_the_plugin() {
        let _main = main_thread();
        let (_client, handle) = loaded();
        handle
            .any_thread()
            .set_parameter_value(ParamId::new(1), Normalized::of(0.25));
        assert_eq!(
            handle
                .any_thread()
                .parameter_value(ParamId::new(1))
                .map(Normalized::get),
            Some(0.25)
        );
    }

    /// **The state saved on the main thread is the plugin's own**: not
    /// `Unavailable` (nothing waited) and not empty.
    #[test]
    fn the_state_saves_on_the_main_thread() {
        let _main = main_thread();
        let (_client, handle) = loaded();
        let state = handle.any_thread().save_state().expect("the probe saves");
        assert!(!state.is_empty());
    }

    /// **The render mode is accepted** (VST2 answers the process level from a
    /// host callback, so there is nothing to decline).
    #[test]
    fn the_render_mode_is_accepted_on_the_main_thread() {
        let _main = main_thread();
        let (_client, handle) = loaded();
        assert!(handle.set_render_mode(RenderMode::Offline));
        assert!(handle.set_render_mode(RenderMode::Realtime));
    }

    /// **No in-process format carries the automation advisory**, and says so
    /// with the editor's `GuiNotSupported` for its format, as the handle with
    /// no such route always did.
    #[test]
    fn the_automation_advisory_is_not_supported_in_process() {
        let _main = main_thread();
        let (_client, handle) = loaded();
        let refused = handle
            .set_automation_mode(AutomationMode::Writing)
            .expect_err("no in-process format carries it");
        assert!(
            matches!(refused, EditorError::GuiNotSupported { ref format } if format == "vst2"),
            "{refused:?}"
        );
    }

    /// **An in-process plugin is never observed dead**: alive, no refusal,
    /// zero block counts, and a `terminate` that ends nothing.
    #[test]
    fn an_in_process_plugin_is_never_observed_dead() {
        let _main = main_thread();
        let (_client, handle) = loaded();
        assert!(!handle.is_crashed());
        assert_eq!(handle.status(), crate::PluginStatus::Alive);
        assert_eq!(
            handle.block_counts(),
            tutti_plugin_types::BlockCounts::default()
        );
        handle.terminate("a test");
        assert_eq!(
            handle.status(),
            crate::PluginStatus::Alive,
            "there is no process to end"
        );
    }

    /// **The rest of the calls have the same unavailable answer when nothing
    /// pumps the main thread**: the catalog and the text are `None`, a render
    /// mode is refused (`false`), never a default that looks like an answer.
    /// (The advisory needs no main thread: it refuses at once.)
    #[test]
    fn an_unpumped_main_thread_refuses_the_other_calls_too() {
        let _main = main_thread();
        let (_client, handle) = loaded();
        let (catalog, text, from_text, mode, automation) = std::thread::spawn(move || {
            (
                handle.any_thread().parameter_descriptors(),
                handle
                    .any_thread()
                    .parameter_text(ParamId::new(1), Normalized::of(0.5)),
                handle
                    .any_thread()
                    .parameter_value_from_text(ParamId::new(1), "0.5"),
                handle.set_render_mode(RenderMode::Offline),
                handle.set_automation_mode(AutomationMode::Off),
            )
        })
        .join()
        .expect("the job returns");
        assert!(catalog.is_none());
        assert!(text.is_none());
        assert!(from_text.is_none());
        assert!(!mode);
        assert!(matches!(
            automation,
            Err(EditorError::GuiNotSupported { .. })
        ));
    }
}
