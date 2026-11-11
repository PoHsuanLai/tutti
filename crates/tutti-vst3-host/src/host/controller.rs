//! The main-thread half of an activation, and the transitions that split a
//! [`Vst3Loaded`] into the two halves and rejoin them.

use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::Arc;
use tutti_plugin_types::MainThread;

use tutti_rt::{DrainMark, DrainWatch};

use tutti_plugin_types::{
    AutomationMode, ContentScale, Control, Controller, Edit, EditSink, EditorSize, EditorWindow,
    Loaded, LoadedPlugin, Normalized, ParamId, ParameterInfo, PluginDescriptor, PluginError,
    PluginResult, PluginTail, Prepare, Preset, PresetId, Processor, Refused, RenderMode, Request,
    RescanScope, RestartRules, SampleFormat, Samples, Stuck, WindowHandle,
};

use crate::types::{ProcessMode, Vst3Sample};

use super::control::plugin_error;
use super::instance::Staged;
use super::loaded::{Activation, Vst3Loaded};
use super::processor::{EDIT_CAPACITY, ProcessorAny, ProcessorParts, Vst3Processor};
use super::shell::Shell;

/// The main-thread half of an activation: the whole control surface of a
/// [`Vst3Loaded`], reached through `Deref`, with parameter writes also queued
/// to the Processor.
///
/// `Send` so a server can build it on one thread and run it on another, not
/// `Sync`, and never used from two threads at once. Dropping it releases its
/// share of the activation; the plugin is destroyed when the Processor's goes
/// too (see the invariant on the shell).
///
/// It does not `Deref` to [`Vst3Loaded`]: the control surface is the
/// [`Control`] trait and the methods here, and the format's own surface is
/// [`loaded`](Self::loaded) and [`loaded_mut`](Self::loaded_mut).
///
/// ```compile_fail
/// // No `Deref`: a `&Vst3Controller` is not a `&Vst3Loaded`.
/// fn loaded(c: &tutti_vst3_host::Vst3Controller) -> &tutti_vst3_host::Vst3Loaded {
///     c
/// }
/// ```
pub struct Vst3Controller {
    /// The plugin of the activation. `None` only once `deactivate` has taken it,
    /// or the drop has handed it on.
    loaded: Option<Vst3Loaded>,
    /// The identity of the pair, and the hand-off to the Processor.
    shell: Arc<Shell>,
    edits: EditSink,
    /// Writes a full ring refused, latest value per parameter, retried on the
    /// next tick ([`flush_edits`](Control::flush_edits)).
    unsent: Vec<(ParamId, Normalized)>,
    /// A [`Edit::Reset`] the full ring refused: sent before anything in `unsent`,
    /// which is everything written since.
    reset_pending: bool,
    /// What the Controller handed the Processor, latest per parameter, until a
    /// block has surely taken it: what a save must add, because the
    /// component's stream does not hold it yet.
    handed: Vec<Handed>,
    /// Reads the Processor's drain count.
    watch: DrainWatch,
    _not_sync: PhantomData<Cell<()>>,
}

/// A write on its way to the Processor.
#[derive(Debug, Clone, Copy)]
struct Handed {
    id: ParamId,
    value: Normalized,
    /// The drain count read after the write entered the ring; `None` while the
    /// ring has refused it.
    sent_at: Option<DrainMark>,
}

impl Handed {
    /// Whether the write has left the ring for the plugin's `process`: two
    /// drains have begun since it entered (`tutti_rt::DrainWatch::two_since`
    /// says why two). That is the ring read, not the plugin's word that it took
    /// the point. A drain that overflows `pending` (more than the reserve's
    /// distinct parameters in one drain, which only a burst from a
    /// parameter-rich plugin can reach) drops the point and counts it in
    /// `edits_dropped`, and the entry goes with the rest: that write is then in
    /// no save. A bus bind that fails after the drain would lose its points
    /// too, but it fails only on a sample-width mismatch, which a typed
    /// Processor cannot have; a `process` that fails was handed the points
    /// first.
    fn is_drained(&self, watch: &DrainWatch) -> bool {
        self.sent_at.is_some_and(|mark| watch.two_since(mark))
    }
}

impl std::fmt::Debug for Vst3Controller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vst3Controller")
            .field("unsent", &self.unsent)
            .finish_non_exhaustive()
    }
}

impl Vst3Controller {
    fn new(
        loaded: Vst3Loaded,
        shell: Arc<Shell>,
        edits: EditSink,
        handed: Vec<Handed>,
        watch: DrainWatch,
    ) -> Self {
        Self {
            loaded: Some(loaded),
            shell,
            edits,
            unsent: Vec::new(),
            reset_pending: false,
            handed,
            watch,
            _not_sync: PhantomData,
        }
    }

    fn shell(&self) -> &Arc<Shell> {
        &self.shell
    }

    /// The loaded plugin, for the surface the [`Control`] impl and this type's
    /// own methods do not cover. The Controller does not `Deref` to it (doc 016
    /// section 2.2).
    ///
    /// # Panics
    ///
    /// Never in use: the plugin is taken only when the Controller is consumed.
    #[must_use]
    pub fn loaded(&self) -> &Vst3Loaded {
        self.loaded
            .as_ref()
            .expect("a live Controller holds its plugin")
    }

    /// [`loaded`](Self::loaded), mutably.
    ///
    /// # Panics
    ///
    /// Never in use: the plugin is taken only when the Controller is consumed.
    pub fn loaded_mut(&mut self) -> &mut Vst3Loaded {
        self.loaded
            .as_mut()
            .expect("a live Controller holds its plugin")
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
        let held_back = self.reset_pending || self.unsent.iter().any(|(waiting, _)| *waiting == id);
        let sent = !held_back && self.edits.send(Edit::Param { id, value }).is_ok();
        if !sent {
            self.keep(id, value);
        }
        let sent_at = sent.then(|| self.watch.mark());
        self.hand(id, value, sent_at);
    }

    /// Records the write as handed to the Processor, replacing an earlier one
    /// to the same parameter.
    fn hand(&mut self, id: ParamId, value: Normalized, sent_at: Option<DrainMark>) {
        let handed = Handed { id, value, sent_at };
        match self.handed.iter_mut().find(|h| h.id == id) {
            Some(slot) => *slot = handed,
            None => self.handed.push(handed),
        }
    }

    /// The writes handed to the Processor that no block has surely taken yet,
    /// and so are in no component stream. Forgets the ones that have been.
    fn undrained(&mut self) -> Vec<(ParamId, Normalized)> {
        let watch = &self.watch;
        self.handed.retain(|h| !h.is_drained(watch));
        self.handed.iter().map(|h| (h.id, h.value)).collect()
    }

    /// Holds a write for the next tick, replacing an earlier one to the same
    /// parameter.
    fn keep(&mut self, id: ParamId, value: Normalized) {
        match self.unsent.iter_mut().find(|(waiting, _)| *waiting == id) {
            Some(slot) => slot.1 = value,
            None => self.unsent.push((id, value)),
        }
    }

    /// Makes every write made so far stale: a state or a preset load replaced
    /// the values they were written against, and the next block must not put
    /// them back. The ring's order makes a [`Edit::Reset`] exact; a full ring
    /// holds it for the next tick, and every write after it waits behind it.
    fn reset_edits(&mut self) {
        self.unsent.clear();
        self.handed.clear();
        self.reset_pending = self.edits.send(Edit::Reset).is_err();
    }
}

impl Drop for Vst3Controller {
    fn drop(&mut self) {
        // Dropped without a rejoin: the plugin is destroyed here, on the
        // control side, unless the Processor still renders, in which case it is
        // parked on the shell for the Processor's drop to retire.
        if let Some(loaded) = self.loaded.take() {
            drop(self.shell.controller_gone(loaded));
        }
    }
}

impl Control for Vst3Controller {
    fn info(&self) -> &PluginDescriptor {
        Control::info(self.loaded())
    }

    fn wiring(&self, main: &MainThread) -> LoadedPlugin {
        Control::wiring(self.loaded(), main)
    }

    fn parameters(&self, main: &MainThread) -> Vec<ParameterInfo> {
        Control::parameters(self.loaded(), main)
    }

    fn parameter(&self, main: &MainThread, id: ParamId) -> Option<Normalized> {
        Control::parameter(self.loaded(), main, id)
    }

    /// Applied to the edit controller now, so a read-back and the plugin's
    /// editor see it, and queued to the Processor, which puts it in the next
    /// block's `inputParameterChanges`: a plugin whose component is separate
    /// from its controller has no other way to hear of it.
    fn set_parameter(&mut self, main: &MainThread, id: ParamId, value: Normalized) {
        Control::set_parameter(self.loaded_mut(), main, id, value);
        self.queue_edit(id, value);
    }

    fn parameter_text(&self, main: &MainThread, id: ParamId, value: Normalized) -> Option<String> {
        Control::parameter_text(self.loaded(), main, id, value)
    }

    fn parameter_value_from_text(
        &self,
        main: &MainThread,
        id: ParamId,
        text: &str,
    ) -> Option<Normalized> {
        Control::parameter_value_from_text(self.loaded(), main, id, text)
    }

    /// Saves the state, with the writes no block has taken yet beside the
    /// component's stream, which does not hold them.
    fn save_state(&mut self, main: &MainThread) -> PluginResult<Vec<u8>> {
        let undrained = self.undrained();
        self.loaded_mut().save_state_with(main, &undrained)
    }

    /// Restores the state, and makes the writes queued before it stale. The
    /// writes the blob carries are queued to the Processor, which the
    /// component's own stream did not hold.
    fn load_state(&mut self, main: &MainThread, bytes: &[u8]) -> PluginResult<()> {
        let result = self.loaded_mut().restore_state(main, bytes);
        self.reset_edits();
        for (id, value) in result? {
            self.queue_edit(id, value);
        }
        Ok(())
    }

    fn presets(&mut self, main: &MainThread) -> Vec<Preset> {
        Control::presets(self.loaded_mut(), main)
    }

    fn load_preset(&mut self, main: &MainThread, id: &PresetId) -> bool {
        match self.loaded_mut().program_write(main, id) {
            Some((param, value)) => {
                // A program replaces the plugin's other parameters too: what
                // was queued before it is stale.
                self.reset_edits();
                Control::set_parameter(self, main, param, value);
                true
            }
            None => false,
        }
    }

    fn current_preset(&mut self, main: &MainThread) -> Option<PresetId> {
        Control::current_preset(self.loaded_mut(), main)
    }

    fn set_automation_state(&mut self, main: &MainThread, mode: AutomationMode) {
        Control::set_automation_state(self.loaded_mut(), main, mode);
    }

    fn rescan_parameters(&mut self, main: &MainThread, scope: RescanScope) {
        Control::rescan_parameters(self.loaded_mut(), main, scope);
    }

    fn take_requests(&mut self, main: &MainThread, out: &mut Vec<Request>) {
        Control::take_requests(self.loaded_mut(), main, out);
    }

    fn set_render_mode(&mut self, main: &MainThread, mode: RenderMode) -> bool {
        Control::set_render_mode(self.loaded_mut(), main, mode)
    }

    fn take_reported_edits(&mut self, main: &MainThread, out: &mut Vec<(ParamId, Normalized)>) {
        Control::take_reported_edits(self.loaded_mut(), main, out);
    }

    fn dropped_reported_edits(&self) -> u64 {
        Control::dropped_reported_edits(self.loaded())
    }

    fn rebuild_midi_mapping(&mut self, main: &MainThread) {
        Control::rebuild_midi_mapping(self.loaded_mut(), main);
    }

    /// Retries what the ring refused, oldest first, and lets the retired
    /// midi-mapping tables go.
    fn flush_edits(&mut self, _main: &MainThread) {
        self.loaded_mut().midi_cc.reclaim();
        // A tick is a control-side moment: plugins whose last holder was a
        // Processor are destroyed here, not on its thread.
        super::shell::reap();
        if self.reset_pending {
            if self.edits.send(Edit::Reset).is_err() {
                return;
            }
            self.reset_pending = false;
        }
        let waiting = std::mem::take(&mut self.unsent);
        for (id, value) in waiting {
            if self.edits.send(Edit::Param { id, value }).is_err() {
                self.unsent.push((id, value));
            } else {
                let sent_at = Some(self.watch.mark());
                self.hand(id, value, sent_at);
            }
        }
    }
}

impl EditorWindow for Vst3Controller {
    fn has_editor(&self, main: &MainThread) -> bool {
        EditorWindow::has_editor(self.loaded(), main)
    }

    fn open_editor(
        &mut self,
        main: &MainThread,
        parent: WindowHandle,
        scale: ContentScale,
    ) -> PluginResult<EditorSize> {
        EditorWindow::open_editor(self.loaded_mut(), main, parent, scale)
    }

    fn close_editor(&mut self, main: &MainThread) {
        EditorWindow::close_editor(self.loaded_mut(), main);
    }

    fn editor_idle(&mut self, main: &MainThread) {
        EditorWindow::editor_idle(self.loaded_mut(), main);
    }

    /// The default capabilities: this surface reports no resize support.
    fn editor_capabilities(
        &mut self,
        _main: &tutti_plugin_types::MainThread,
    ) -> tutti_plugin_types::EditorCapabilities {
        tutti_plugin_types::EditorCapabilities::default()
    }

    /// Not supported here.
    fn set_editor_size(
        &mut self,
        _main: &tutti_plugin_types::MainThread,
        _requested: tutti_plugin_types::EditorSize,
    ) -> tutti_plugin_types::PluginResult<tutti_plugin_types::EditorSize> {
        Err(tutti_plugin_types::PluginError::Editor(
            tutti_plugin_types::EditorError::PluginError("set_editor_size not supported".into()),
        ))
    }

    /// Nothing asked: this surface never queues a resize request.
    fn poll_editor_resize_request(
        &mut self,
        _main: &tutti_plugin_types::MainThread,
    ) -> Option<tutti_plugin_types::EditorSize> {
        None
    }
}

impl Controller for Vst3Controller {
    type Loaded = Vst3Loaded;
    type Processor = ProcessorAny;

    fn latency(&self, main: &MainThread) -> Samples {
        Samples(self.loaded().latency_samples(main) as usize)
    }

    fn tail(&self, main: &MainThread) -> PluginTail {
        PluginTail::from_vst3_samples(self.loaded().tail_samples(main))
    }
}

impl Vst3Loaded {
    /// Activates the plugin at a sample format fixed by `T`, splitting it into
    /// the realtime [`Vst3Processor<T>`] and the main-thread
    /// [`Vst3Controller`]. This is [`Loaded::activate`] for a user who fixes the
    /// sample type at compile time; the trait's version picks `T` from
    /// [`Prepare::format`] and wraps the processor in [`ProcessorAny`].
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
    // The `Err` variant deliberately hands `self` (a large `Vst3Loaded`) back so
    // the caller can retry or fall back; boxing it would defeat that ownership
    // return and add a heap alloc on the (rare) failure path.
    #[allow(clippy::result_large_err)]
    pub fn activate_typed<T: Vst3Sample>(
        mut self,
        main: &MainThread,
        prepare: Prepare,
    ) -> std::result::Result<(Vst3Processor<T>, Vst3Controller), Refused<Self>> {
        // `maxSamplesPerBlock` is an `i32`, narrower than a `MaxBlock`'s `u32`.
        if i32::try_from(prepare.max_block().get_u32()).is_err() {
            return Err(Refused {
                plugin: Some(self),
                reason: PluginError::Other(format!(
                    "a block of {} frames exceeds VST3's maxSamplesPerBlock (i32)",
                    prepare.max_block().get()
                )),
            });
        }
        super::shell::warm_retirement();
        super::shell::reap();
        let mode = if prepare.mode().is_offline() {
            ProcessMode::Offline
        } else {
            ProcessMode::Realtime
        };
        // What an earlier rejoin carried over reaches the plugin in the first
        // block, unless this activation is refused, which keeps it.
        let carried = std::mem::take(&mut self.carried);
        let staged = match Staged::<T>::stage(
            main,
            self,
            prepare.rate().0,
            prepare.max_block().get(),
            mode,
        ) {
            Ok(staged) => staged,
            Err((mut plugin, error)) => {
                plugin.carried = carried;
                return Err(Refused {
                    plugin: Some(plugin),
                    reason: plugin_error(error),
                });
            }
        };
        let Staged { loaded, audio, .. } = staged;
        // The plugin may announce its latency from inside `setActive`; the
        // figure is read right after every activation, so the announcement is
        // answered. Everything else raised meanwhile is kept.
        for request in tutti_plugin_types::taken(loaded.plugin.handler().requests()) {
            if request != Request::LatencyChanged {
                let _ = request.raise(loaded.plugin.handler().requests());
            }
        }
        let Some(audio_side) = loaded.plugin.audio_side() else {
            return Err(Refused {
                plugin: Some(loaded),
                reason: PluginError::Other(
                    "the plugin is not set up and active, or its audio half is out".into(),
                ),
            });
        };
        let context_requirements = loaded.context_requirements();
        let shell = Arc::new(Shell::new());
        let (sink, drain) = tutti_plugin_types::edits(EDIT_CAPACITY);
        let (beat, watch) = tutti_rt::drain_clock();
        let handed = carried
            .iter()
            .map(|&(id, value)| Handed {
                id,
                value,
                sent_at: Some(watch.mark()),
            })
            .collect();
        let processor = Vst3Processor::new(ProcessorParts {
            shell: shell.clone(),
            audio,
            audio_side,
            context_requirements,
            drain,
            beat,
            carried,
        });
        Ok((
            processor,
            Vst3Controller::new(loaded, shell, sink, handed, watch),
        ))
    }

    /// Rejoins the halves of one activation of any sample type: checks they are
    /// a pair, stops the Processor, drops it, and hands back the plugin
    /// deactivated, with the writes no block took waiting for the next
    /// activation.
    #[allow(clippy::result_large_err)]
    fn rejoin<P>(
        main: &MainThread,
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
        let mut loaded = controller
            .loaded
            .take()
            .expect("a Controller holds its plugin until it is consumed");
        controller.shell.controller_rejoined();
        loaded.deactivate_plugin(main);
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
        main: &MainThread,
        processor: Vst3Processor<T>,
        controller: Vst3Controller,
    ) -> Result<Vst3Loaded, (Vst3Processor<T>, Vst3Controller, PluginError)> {
        Self::rejoin(
            main,
            processor,
            controller,
            |p| p.shell(),
            Vst3Processor::stop,
            Vst3Processor::take_edits,
        )
    }

    /// `setActive(0)`, if the plugin is active. The Processor has stopped
    /// processing by now (`rejoin` stops it, and sends `setProcessing(0)` only
    /// if it was on), so it is not told twice.
    fn deactivate_plugin(&mut self, main: &MainThread) {
        if !self.active.is_active() {
            return;
        }
        // A refusal of `setActive(0)` leaves nothing this host could act on:
        // the interfaces are still valid and every loaded-state method is still
        // legal. (The asymmetry with `setActive(1)`, where a refusal is a
        // failure.)
        let _ = self.plugin.set_active(main, false);
        self.active = Activation::default();
    }
}

/// `pending` with each parameter once, at its latest value, in the order each
/// parameter was first written.
pub(super) fn latest_per_parameter(
    pending: Vec<(ParamId, Normalized)>,
) -> Vec<(ParamId, Normalized)> {
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
    type Processor = ProcessorAny;
    type Controller = Vst3Controller;

    /// VST3 restarts for latency and for the buses (`kIoChanged`).
    const RESTARTS: RestartRules = RestartRules::VST3;

    fn activate(
        self,
        main: &MainThread,
        prepare: Prepare,
    ) -> Result<(ProcessorAny, Vst3Controller), Refused<Self>> {
        match prepare.format() {
            SampleFormat::Float64 => self
                .activate_typed::<f64>(main, prepare)
                .map(|(p, c)| (ProcessorAny::F64(p), c)),
            _ => self
                .activate_typed::<f32>(main, prepare)
                .map(|(p, c)| (ProcessorAny::F32(p), c)),
        }
    }

    fn deactivate(
        main: &MainThread,
        processor: ProcessorAny,
        controller: Vst3Controller,
    ) -> Result<Self, Stuck<ProcessorAny, Vst3Controller>> {
        Self::rejoin(
            main,
            processor,
            controller,
            |p| p.shell(),
            Processor::stop,
            ProcessorAny::take_edits,
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
    fn rebuild(mut self, main: &MainThread) -> Result<Self, Refused<Self>> {
        let state = self.save_state(main).ok();
        let options = match self.origin.load().chosen() {
            Some(class) => super::loaded::LoadOptions::new().class(class),
            None => super::loaded::LoadOptions::new(),
        };
        let mut fresh = match Vst3Loaded::load(main, self.origin.path(), options) {
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
            let _ = fresh.load_state(main, &state);
        }
        fresh.carried = std::mem::take(&mut self.carried);
        Ok(fresh)
    }
}
