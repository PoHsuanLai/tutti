//! The main-thread half of an activation, and the transitions that split a
//! [`Vst3Loaded`] into the two halves and rejoin them.

use std::cell::Cell;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use vst3::Steinberg::Vst::{IAudioProcessorTrait, IComponentTrait};

use tutti_plugin_types::{
    AutomationMode, Control, Controller, Edit, EditSink, Loaded, LoadedPlugin, Normalized, ParamId,
    ParameterInfo, PluginDescriptor, PluginError, PluginResult, PluginTail, Prepare, Preset,
    PresetId, Processor, Refused, Request, RescanScope, RestartRules, SampleFormat, Samples, Stuck,
};

use crate::types::{ProcessMode, Vst3Sample};

use super::control::plugin_error;
use super::instance::Staged;
use super::loaded::{Activation, Vst3Loaded};
use super::processor::{EDIT_CAPACITY, ProcessorParts, Vst3Processor, Vst3ProcessorAny};
use super::shell::Shell;

/// The main-thread half of an activation: the whole control surface of a
/// [`Vst3Loaded`], reached through `Deref`, with parameter writes also queued
/// to the Processor.
///
/// `Send` so a server can build it on one thread and run it on another, not
/// `Sync`, and never used from two threads at once. Dropping it releases its
/// share of the activation; the plugin is destroyed when the Processor's goes
/// too (see the invariant on the shell).
pub struct Vst3Controller {
    /// `None` only once `deactivate` has taken the shell.
    shell: Option<Arc<Shell>>,
    edits: EditSink,
    /// Writes a full ring refused, latest value per parameter, retried on the
    /// next tick ([`flush_edits`](Control::flush_edits)).
    unsent: Vec<(ParamId, Normalized)>,
    _not_sync: PhantomData<Cell<()>>,
}

impl std::fmt::Debug for Vst3Controller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vst3Controller")
            .field("unsent", &self.unsent)
            .finish_non_exhaustive()
    }
}

// SAFETY: the Controller runs on one thread at a time (every method takes
// `&mut self` or a shared reference of a type that is not `Sync`), and clause 2
// of the invariant on `Shell` keeps its calls to the main-thread set.
unsafe impl Send for Vst3Controller {}

impl Vst3Controller {
    fn new(shell: Arc<Shell>, edits: EditSink) -> Self {
        Self {
            shell: Some(shell),
            edits,
            unsent: Vec::new(),
            _not_sync: PhantomData,
        }
    }

    fn shell(&self) -> &Arc<Shell> {
        self.shell
            .as_ref()
            .expect("a Controller holds its shell until it is consumed")
    }

    fn loaded(&self) -> &Vst3Loaded {
        // SAFETY: main thread, shared; see `Shell::loaded`.
        unsafe { self.shell().loaded() }
            .as_ref()
            .expect("a live Controller's shell holds its plugin")
    }

    /// Edits the ring refused for want of room, cumulatively. Each was kept and
    /// retried; the count says the Processor was not draining as fast as the
    /// Controller wrote.
    #[must_use]
    pub fn refused_edits(&self) -> u32 {
        self.edits.refused()
    }

    /// Queues a parameter write to the Processor.
    ///
    /// A value already waiting to be retried is replaced by the newer one; a
    /// full ring keeps the value for the next tick.
    fn queue_edit(&mut self, id: ParamId, value: Normalized) {
        if let Some(slot) = self.unsent.iter_mut().find(|(waiting, _)| *waiting == id) {
            slot.1 = value;
            return;
        }
        if self.edits.send(Edit::Param { id, value }).is_err() {
            self.unsent.push((id, value));
        }
    }
}

impl Deref for Vst3Controller {
    type Target = Vst3Loaded;

    fn deref(&self) -> &Vst3Loaded {
        self.loaded()
    }
}

impl DerefMut for Vst3Controller {
    fn deref_mut(&mut self) -> &mut Vst3Loaded {
        let shell = self.shell();
        // SAFETY: main thread, exclusive; see `Shell::loaded_mut`.
        unsafe { shell.loaded_mut() }
            .as_mut()
            .expect("a live Controller's shell holds its plugin")
    }
}

impl Control for Vst3Controller {
    fn info(&self) -> &PluginDescriptor {
        Control::info(&**self)
    }

    fn wiring(&self) -> LoadedPlugin {
        Control::wiring(&**self)
    }

    fn parameters(&self) -> Vec<ParameterInfo> {
        Control::parameters(&**self)
    }

    fn parameter(&self, id: ParamId) -> Option<Normalized> {
        Control::parameter(&**self, id)
    }

    /// Applied to the edit controller now, so a read-back and the plugin's
    /// editor see it, and queued to the Processor, which puts it in the next
    /// block's `inputParameterChanges`: a plugin whose component is separate
    /// from its controller has no other way to hear of it.
    fn set_parameter(&mut self, id: ParamId, value: Normalized) {
        Control::set_parameter(&mut **self, id, value);
        self.queue_edit(id, value);
    }

    fn parameter_text(&self, id: ParamId, value: Normalized) -> Option<String> {
        Control::parameter_text(&**self, id, value)
    }

    fn parameter_value_from_text(&self, id: ParamId, text: &str) -> Option<Normalized> {
        Control::parameter_value_from_text(&**self, id, text)
    }

    fn save_state(&mut self) -> PluginResult<Vec<u8>> {
        Control::save_state(&mut **self)
    }

    fn load_state(&mut self, bytes: &[u8]) -> PluginResult<()> {
        Control::load_state(&mut **self, bytes)
    }

    fn presets(&mut self) -> Vec<Preset> {
        Control::presets(&mut **self)
    }

    fn load_preset(&mut self, id: &PresetId) -> bool {
        match self.program_write(id) {
            Some((param, value)) => {
                Control::set_parameter(self, param, value);
                true
            }
            None => false,
        }
    }

    fn current_preset(&mut self) -> Option<PresetId> {
        Control::current_preset(&mut **self)
    }

    fn set_automation_state(&mut self, mode: AutomationMode) {
        Control::set_automation_state(&mut **self, mode);
    }

    fn rescan_parameters(&mut self, scope: RescanScope) {
        Control::rescan_parameters(&mut **self, scope);
    }

    fn rebuild_midi_mapping(&mut self) {
        Control::rebuild_midi_mapping(&mut **self);
    }

    /// Retries what the ring refused, oldest first, and lets the retired
    /// midi-mapping tables go.
    fn flush_edits(&mut self) {
        self.midi_cc.reclaim();
        let waiting = std::mem::take(&mut self.unsent);
        for (id, value) in waiting {
            if self.edits.send(Edit::Param { id, value }).is_err() {
                self.unsent.push((id, value));
            }
        }
    }
}

impl Controller for Vst3Controller {
    type Loaded = Vst3Loaded;
    type Processor = Vst3ProcessorAny;

    fn latency(&self) -> Samples {
        Samples(self.latency_samples() as usize)
    }

    fn tail(&self) -> PluginTail {
        PluginTail::from_vst3_samples(self.tail_samples())
    }
}

impl Vst3Loaded {
    /// Activates the plugin at a sample format fixed by `T`, splitting it into
    /// the realtime [`Vst3Processor<T>`] and the main-thread
    /// [`Vst3Controller`]. This is [`Loaded::activate`] for a user who fixes the
    /// sample type at compile time; the trait's version picks `T` from
    /// [`Prepare::format`] and wraps the processor in [`Vst3ProcessorAny`].
    ///
    /// Runs `setBusArrangements`, `setupProcessing` (the render mode is
    /// [`Prepare::mode`]: VST3 delivers it here and nowhere else, so a change
    /// of mode is a new activation), `activateBus` and `setActive(1)`, and sizes
    /// the scratch from what the plugin settled on.
    ///
    /// # Errors
    ///
    /// [`Refused`] carries the **unconsumed** `Vst3Loaded` beside the reason, so
    /// a refusal costs the configuration and not the instance: a plugin that
    /// declines 64-bit audio is retried at `f32` against the same instance.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if called off the thread registered with
    /// [`tutti_plugin_types::mark_main_thread`].
    // The `Err` variant deliberately hands `self` (a large `Vst3Loaded`) back so
    // the caller can retry or fall back; boxing it would defeat that ownership
    // return and add a heap alloc on the (rare) failure path.
    #[allow(clippy::result_large_err)]
    pub fn activate_typed<T: Vst3Sample>(
        mut self,
        prepare: Prepare,
    ) -> std::result::Result<(Vst3Processor<T>, Vst3Controller), Refused<Self>> {
        tutti_plugin_types::assert_main_thread();
        let mode = if prepare.mode.is_offline() {
            ProcessMode::Offline
        } else {
            ProcessMode::Realtime
        };
        // What an earlier rejoin carried over reaches the plugin in the first
        // block, unless this activation is refused, which keeps it.
        let carried = std::mem::take(&mut self.carried);
        let staged = match Staged::<T>::stage(self, prepare.rate.0, prepare.max_block.0, mode) {
            Ok(staged) => staged,
            Err((mut plugin, error)) => {
                plugin.carried = carried;
                return Err(Refused {
                    plugin: Some(plugin),
                    reason: plugin_error(error),
                });
            }
        };
        let Staged { loaded, audio } = staged;
        // The plugin may announce its latency from inside `setActive`; the
        // figure is read right after every activation, so the announcement is
        // answered. Everything else raised meanwhile is kept.
        for request in tutti_plugin_types::taken(loaded.host.handler.requests()) {
            if request != Request::LatencyChanged {
                let _ = request.raise(loaded.host.handler.requests());
            }
        }
        let processor = loaded.interfaces.processor.clone();
        let context_requirements = loaded.interfaces.process_context_requirements;
        let shell = Arc::new(Shell::new(loaded));
        let (sink, drain) = tutti_plugin_types::edits(EDIT_CAPACITY);
        let processor = Vst3Processor::new(ProcessorParts {
            shell: shell.clone(),
            audio,
            processor,
            context_requirements,
            drain,
            carried: carried.into_iter().map(|(id, v)| (id.get(), v)).collect(),
        });
        Ok((processor, Vst3Controller::new(shell, sink)))
    }

    /// Rejoins the halves of one activation of any sample type: checks they are
    /// a pair, stops the Processor, drops it, and hands back the plugin
    /// deactivated, with the writes no block took waiting for the next
    /// activation.
    fn rejoin<P>(
        mut processor: P,
        mut controller: Vst3Controller,
        shell_of: impl Fn(&P) -> &Arc<Shell>,
        stop: impl FnOnce(&mut P),
        take_edits: impl FnOnce(&mut P, &mut Vec<(ParamId, Normalized)>),
    ) -> Result<Vst3Loaded, (P, Vst3Controller, PluginError)> {
        if !Arc::ptr_eq(shell_of(&processor), controller.shell()) {
            return Err((
                processor,
                controller,
                PluginError::Other("the halves are of two activations".into()),
            ));
        }
        // The pair and nothing else: one share each.
        if Arc::strong_count(controller.shell()) != 2 {
            return Err((
                processor,
                controller,
                PluginError::Other("the activation has a third handle".into()),
            ));
        }
        stop(&mut processor);
        // Writes no block has drained would die with the ring. Older first: what
        // the ring held, then what a full ring had refused.
        let mut pending = Vec::new();
        take_edits(&mut processor, &mut pending);
        pending.append(&mut std::mem::take(&mut controller.unsent));
        drop(processor);
        let shell = controller
            .shell
            .take()
            .expect("a Controller holds its shell until it is consumed");
        let shell = Arc::try_unwrap(shell)
            .unwrap_or_else(|_| unreachable!("the pair was the only two handles"));
        let mut loaded = shell
            .into_loaded()
            .expect("the plugin stays in the shell until the last half drops");
        loaded.deactivate_plugin();
        loaded.carried = latest_per_parameter(pending);
        Ok(loaded)
    }

    /// [`Loaded::deactivate`] for a processor of a fixed sample type, as
    /// [`activate_typed`](Self::activate_typed) made it.
    ///
    /// # Errors
    ///
    /// The halves come back, with the reason, when they are of two activations.
    #[allow(clippy::result_large_err)]
    pub fn deactivate_typed<T: Vst3Sample>(
        processor: Vst3Processor<T>,
        controller: Vst3Controller,
    ) -> Result<Vst3Loaded, (Vst3Processor<T>, Vst3Controller, PluginError)> {
        Self::rejoin(
            processor,
            controller,
            |p| p.shell(),
            Vst3Processor::stop,
            Vst3Processor::take_edits,
        )
    }

    /// `setProcessing(0)` and `setActive(0)`, if the plugin is active.
    fn deactivate_plugin(&mut self) {
        if !self.active.active {
            return;
        }
        // A refusal of `setActive(0)` leaves nothing this host could act on:
        // the interfaces are still valid and every loaded-state method is still
        // legal. (The asymmetry with `setActive(1)`, where a refusal is a
        // failure.)
        unsafe {
            self.interfaces.processor.setProcessing(0);
            let _ = self.interfaces.component.setActive(0);
        }
        self.active = Activation::default();
    }
}

/// `pending` with each parameter once, at its latest value, in the order each
/// parameter was first written.
fn latest_per_parameter(pending: Vec<(ParamId, Normalized)>) -> Vec<(ParamId, Normalized)> {
    let mut out: Vec<(ParamId, Normalized)> = Vec::with_capacity(pending.len());
    for (id, value) in pending {
        match out.iter_mut().find(|(seen, _)| *seen == id) {
            Some(slot) => slot.1 = value,
            None => out.push((id, value)),
        }
    }
    out
}

impl Loaded for Vst3Loaded {
    type Processor = Vst3ProcessorAny;
    type Controller = Vst3Controller;

    /// VST3 restarts for latency and for the buses (`kIoChanged`).
    const RESTARTS: RestartRules = RestartRules::VST3;

    fn activate(
        self,
        prepare: Prepare,
    ) -> Result<(Vst3ProcessorAny, Vst3Controller), Refused<Self>> {
        match prepare.format {
            SampleFormat::Float64 => self
                .activate_typed::<f64>(prepare)
                .map(|(p, c)| (Vst3ProcessorAny::F64(p), c)),
            _ => self
                .activate_typed::<f32>(prepare)
                .map(|(p, c)| (Vst3ProcessorAny::F32(p), c)),
        }
    }

    fn deactivate(
        processor: Vst3ProcessorAny,
        controller: Vst3Controller,
    ) -> Result<Self, Stuck<Vst3ProcessorAny, Vst3Controller>> {
        Self::rejoin(
            processor,
            controller,
            |p| p.shell(),
            Processor::stop,
            Vst3ProcessorAny::take_edits,
        )
        .map_err(|(processor, controller, reason)| Stuck {
            processor,
            controller,
            reason,
        })
    }

    /// Loads the plugin again from where it came, state and all, and drops the
    /// old instance (`kReloadComponent`). The new instance is built first, so a
    /// refusal leaves the old one in place.
    fn rebuild(mut self) -> Result<Self, Refused<Self>> {
        let state = self.save_state().ok();
        let options = match &self.origin.class {
            Some(class) => super::loaded::LoadOptions::new().class(class.clone()),
            None => super::loaded::LoadOptions::new(),
        };
        let mut fresh = match Vst3Loaded::load(&self.origin.path, options) {
            Ok(fresh) => fresh,
            Err(error) => {
                return Err(Refused {
                    plugin: Some(self),
                    reason: plugin_error(error),
                });
            }
        };
        if let Some(state) = state {
            // A plugin that refuses its own state leaves the fresh instance at
            // its defaults.
            let _ = fresh.load_state(&state);
        }
        fresh.carried = std::mem::take(&mut self.carried);
        Ok(fresh)
    }
}
