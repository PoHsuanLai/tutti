//! The traits a plugin format implements, and the values its transitions
//! exchange.
//!
//! A plugin is [`Loaded`] (configured, not rendering). [`Loaded::activate`]
//! splits it into a [`Processor`], which only the realtime thread holds and
//! which only renders, and a [`Controller`], which the main thread holds and
//! which carries the control surface. [`Loaded::deactivate`] rejoins the
//! halves. The traits are deliberately not object-safe: a transition takes its
//! values by move, so a stale handle cannot be named.
//!
//! [`Control`] is the part of the surface that exists in every state that has a
//! plugin, so `Loaded` and `Controller` both implement it and a caller reaches
//! parameters and state in either.
//!
//! # Threads
//!
//! Every method of [`Control`] and [`Controller`] runs on the main thread, one
//! at a time. The entries that must (all of [`Control`] but `info`, and the
//! transitions of [`Loaded`]) take a [`&MainThread`](crate::MainThread): holding
//! one is the proof, and no check runs. [`Processor::process`] and [`Processor::stop`] run on whichever
//! thread holds the Processor and are the only calls that thread makes. An
//! implementation's `unsafe` states, per format, why the two halves may reach
//! one plugin instance at once.

use std::num::NonZeroUsize;

use crate::ParamEdit;
use crate::{
    AudioBufferMut, AutomationMode, ContentScale, EditorCapabilities, EditorSize, EditorWindow,
    LoadedPlugin, Normalized, ParamId, ParameterInfo, PluginDescriptor, PluginError, PluginResult,
    PluginTail, Preset, PresetId, ProcessContext, ProcessOutput, SampleRate, Samples, WindowHandle,
};
use crate::{FdId, MainThread, Prepare, RenderMode, Request, RescanScope, RestartRules, TimerId};

/// What the main thread asks of a plugin, in any state that has one.
///
/// Object-safe: [`Lifecycle::control`](crate::Lifecycle::control) lends it as
/// `&mut dyn Control`.
pub trait Control: EditorWindow {
    /// The plugin's catalog identity.
    fn info(&self) -> &PluginDescriptor;

    /// The wiring read from the plugin now, not a load-time cache.
    fn wiring(&self, main: &MainThread) -> LoadedPlugin;

    /// Every parameter the plugin declares.
    fn parameters(&self, main: &MainThread) -> Vec<ParameterInfo>;

    /// How many parameters the plugin declares: at least `parameters().len()`
    /// (a format whose list skips a parameter it declines to describe still
    /// counts it). The default builds the list; a format with a count query
    /// overrides it, for a caller between blocks that sizes storage by it and
    /// should not build every descriptor to do so.
    fn parameter_count(&self, main: &MainThread) -> usize {
        self.parameters(main).len()
    }

    /// The parameter's current value, or `None` if the plugin has no such
    /// parameter.
    fn parameter(&self, main: &MainThread, id: ParamId) -> Option<Normalized>;

    /// Sets a parameter. Loaded: applied now. Active: an edit queued to the
    /// Processor, applied at the head of its next block
    /// ([`Edit`](crate::Edit)).
    fn set_parameter(&mut self, main: &MainThread, id: ParamId, value: Normalized);

    /// The plain value `value` stands for, in the unit
    /// [`ParameterInfo::unit`](crate::ParameterInfo::unit) names: the
    /// **format's** conversion, never a formula over the declared range (VST3
    /// asks the controller's `normalizedParamToPlain`, whose shape is the
    /// plugin's; CLAP and AU convert against their own declared range). `None`
    /// when the parameter does not exist or the format has no plain value for
    /// it (a VST2 parameter with no integer range).
    fn parameter_plain(&self, main: &MainThread, id: ParamId, value: Normalized) -> Option<f64>;

    /// The inverse of [`parameter_plain`](Self::parameter_plain): the
    /// [`Normalized`] value a plain one stands for, clamped onto `0..=1` where
    /// the plain value is outside what the plugin declares. `None` as for
    /// `parameter_plain`.
    fn parameter_normalized(
        &self,
        main: &MainThread,
        id: ParamId,
        plain: f64,
    ) -> Option<Normalized>;

    /// The plugin's display text for `value`, if it has any.
    fn parameter_text(&self, main: &MainThread, id: ParamId, value: Normalized) -> Option<String> {
        let _ = (main, id, value);
        None
    }

    /// The value the plugin parses from `text`, if it can.
    fn parameter_value_from_text(
        &self,
        main: &MainThread,
        id: ParamId,
        text: &str,
    ) -> Option<Normalized> {
        let _ = (main, id, text);
        None
    }

    /// The plugin's state as bytes.
    ///
    /// # Errors
    ///
    /// [`PluginError::State`] when the plugin cannot save.
    fn save_state(&mut self, main: &MainThread) -> PluginResult<Vec<u8>>;

    /// Restores state saved by [`save_state`](Self::save_state).
    ///
    /// # Errors
    ///
    /// [`PluginError::State`] when the plugin rejects the bytes.
    fn load_state(&mut self, main: &MainThread, bytes: &[u8]) -> PluginResult<()>;

    /// The plugin's factory presets. Empty when the format has none.
    fn presets(&mut self, main: &MainThread) -> Vec<Preset> {
        let _ = main;
        Vec::new()
    }

    /// Loads a preset; `false` when the format or the plugin cannot.
    fn load_preset(&mut self, main: &MainThread, id: &PresetId) -> bool {
        let _ = (main, id);
        false
    }

    /// The preset the plugin is on, if it says.
    fn current_preset(&mut self, main: &MainThread) -> Option<PresetId> {
        let _ = main;
        None
    }

    /// Tells the plugin how the host is automating it.
    fn set_automation_state(&mut self, main: &MainThread, mode: AutomationMode) {
        let _ = (main, mode);
    }

    /// Reads parameters again after a rescan the plugin asked for.
    fn rescan_parameters(&mut self, main: &MainThread, scope: RescanScope) {
        let _ = (main, scope);
    }

    /// Rebuilds the host's MIDI controller-to-parameter mapping from the plugin
    /// (VST3 `kMidiCCAssignmentChanged`).
    fn rebuild_midi_mapping(&mut self, main: &MainThread) {
        let _ = main;
    }

    /// Retries the parameter edits an active plugin's full ring refused,
    /// latest value per parameter. [`Lifecycle::drive`](crate::Lifecycle::drive)
    /// calls it on every [`Event::Tick`](crate::Event::Tick), so the host must
    /// keep ticking: with no ticks a refused edit is never retried and the
    /// plugin keeps the older value.
    fn flush_edits(&mut self, main: &MainThread) {
        let _ = main;
    }

    /// The plugin's own main-thread work (CLAP `on_main_thread`).
    fn run_main_thread(&mut self, main: &MainThread) {
        let _ = main;
    }

    /// Runs a plugin timer that is due.
    fn run_timer(&mut self, main: &MainThread, id: TimerId) {
        let _ = (main, id);
    }

    /// Services a plugin file descriptor that is ready.
    fn service_fd(&mut self, main: &MainThread, id: FdId) {
        let _ = (main, id);
    }

    /// The shell's one pass over what the plugin asked for since the last call,
    /// appended to `out` in the order [`taken`](crate::taken) yields them, for
    /// [`Lifecycle::drive`](crate::Lifecycle::drive). A format with nothing to
    /// ask says so by pushing nothing.
    fn take_requests(&mut self, main: &MainThread, out: &mut Vec<Request>) {
        let _ = (main, out);
    }

    /// Tells the plugin its render mode now. `true` when the format applied it
    /// without a restart; `false` when it cannot, and the caller reconfigures
    /// through the [`Lifecycle`](crate::Lifecycle), whose
    /// [`Prepare::mode`](crate::Prepare::mode) the next activation carries. A
    /// `false` changes nothing.
    fn set_render_mode(&mut self, main: &MainThread, mode: RenderMode) -> bool {
        let _ = (main, mode);
        false
    }

    /// Appends the parameter edits the plugin reported of its own accord (its
    /// editor moving a knob, its own automation), oldest first, to `out`: each
    /// a [`ParamEdit`], so a gesture's `Begin`, its changes and its `End` come
    /// in the order the plugin made them (doc 029 section 6.1).
    ///
    /// Every format reports them here: VST2's `audioMasterBeginEdit`,
    /// `audioMasterAutomate` and `audioMasterEndEdit`, VST3's `beginEdit`,
    /// `performEdit` and `endEdit` through the host's component handler (and
    /// what the processor wrote to `outputParameterChanges`), CLAP's gesture
    /// and value events from `process` or `params.flush`, AU's parameter
    /// listener. A plugin's echo of a value the host just set is not an edit.
    ///
    /// A served plugin (`Ipc`) reports none here: its server forwards them over
    /// the wire, and the bridge's listener hears each as it arrives.
    fn take_reported_edits(&mut self, main: &MainThread, out: &mut Vec<ParamEdit>) {
        let _ = (main, out);
    }

    /// How many of the edits [`take_reported_edits`](Self::take_reported_edits)
    /// delivers the host lost, over the plugin's life: the reports a bounded
    /// queue refused because nobody drained it. Monotonic. Required, because
    /// zero is a claim: a format that reports edits in the block's
    /// [`ProcessOutput`] says `0` in its own impl.
    fn dropped_reported_edits(&self) -> u64;
}

/// A plugin that is configured but not rendering.
pub trait Loaded: Control + Sized + Send {
    /// The realtime half.
    type Processor: Processor;
    /// The main-thread half.
    type Controller: Controller<Loaded = Self, Processor = Self::Processor>;

    /// What this format needs a restart for. A reload restarts on every format.
    const RESTARTS: RestartRules;

    /// Splits the plugin. On failure the whole plugin comes back in the error.
    ///
    /// # Errors
    ///
    /// [`Refused`] when the plugin will not run at `prepare`.
    fn activate(
        self,
        main: &MainThread,
        prepare: Prepare,
    ) -> Result<(Self::Processor, Self::Controller), Refused<Self>>;

    /// Rejoins the halves of one activation.
    ///
    /// # Errors
    ///
    /// [`Stuck`], with both halves, for a pair from two activations or a plugin
    /// that will not stop.
    fn deactivate(
        main: &MainThread,
        processor: Self::Processor,
        controller: Self::Controller,
    ) -> Result<Self, Stuck<Self::Processor, Self::Controller>>;

    /// Rebuilds the instance from its load parameters (VST3
    /// `kReloadComponent`).
    ///
    /// # Errors
    ///
    /// [`Refused`], holding the plugin if it survived, when the instance cannot
    /// be rebuilt.
    fn rebuild(self, main: &MainThread) -> Result<Self, Refused<Self>> {
        let _ = main;
        Ok(self)
    }
}

/// The realtime half of an active plugin: rendering blocks.
///
/// `Send` to move to the realtime thread, not `Sync`.
pub trait Processor: Send {
    /// Renders one block. Realtime: no allocation, no lock, no blocking. The
    /// error is built only on the failed-block path.
    ///
    /// An implementation drains its edit ring at the head of the block and
    /// merges the edits with `io.ctx`'s automation in scratch it preallocated at
    /// activation, so each Processor owns its own and nothing is built here.
    ///
    /// # Errors
    ///
    /// [`PluginError::Process`] when the plugin failed the block.
    fn process(&mut self, io: &mut PluginIo<'_>) -> PluginResult<()>;

    /// Ends processing (CLAP `stop_processing`, VST3 `setProcessing(false)`) so
    /// the realtime thread can hand the Processor back. Realtime. Processing
    /// starts again lazily in the first [`process`](Self::process).
    fn stop(&mut self);

    /// Asks the plugin to forget its signal history, as a seek or a transport
    /// reset needs: a delay line, a reverb tail, a filter's memory. Realtime,
    /// between blocks.
    ///
    /// Per format: CLAP `plugin.reset`; VST3 `setProcessing(false)`, started
    /// again in the next block (the SDK calls it a way to reset buffers, a
    /// request a plugin may ignore); VST2 `effStopProcess`, started again in the next
    /// block (likewise a request). Every format drops a SysEx open across the reset
    /// (design 016 §6.2).
    fn reset(&mut self);

    /// The frames this Processor renders in, if it renders in fixed chunks
    /// that may begin in the middle of a graph block and span several
    /// [`process`](Self::process) calls (the served pipeline's: it ships a
    /// whole chunk to another process and plays the one before). `None`, the
    /// default, for a Processor that renders whatever block it is handed.
    ///
    /// With `Some(n)` the graph node cuts each block at the Processor's chunk
    /// edges, counted in frames from `prepare` and from every
    /// [`reset`](Self::reset), and calls `process` once per piece: each call
    /// is inside one chunk. What a call carries:
    ///
    /// - **Per piece, on the piece's frames** (from its start): the audio,
    ///   the MIDI ([`ProcessContext::midi_events`]) and the harmony
    ///   ([`ProcessContext::expressive`]). The Processor accumulates these
    ///   into its chunk itself, at the piece's place in it.
    /// - **Per chunk, so far, on the chunk's frames** (from the chunk's first):
    ///   the parameter changes ([`ProcessContext::param_changes`]) and the
    ///   transport. [`ProcessContext::transport`] is the transport at the
    ///   **chunk's** first frame in every piece, and
    ///   [`transport_changes`](ProcessContext::transport_changes) are the
    ///   chunk's changes up to and including the piece, one walk across its
    ///   pieces: a stop, a seek or a tempo-map point on a block boundary inside
    ///   the chunk is a change at its chunk frame. Both are final in the
    ///   piece that **completes** the chunk (the transport's `tempo_inc` is the
    ///   slope to the chunk's end, known only then), which is the call a
    ///   Processor that ships the chunk there reads them in.
    ///
    /// Read each block, on the realtime thread: it must not change while the
    /// Processor renders.
    fn chunk(&self) -> Option<NonZeroUsize> {
        None
    }

    /// The frames of latency this Processor adds to the plugin's own (the
    /// [`Controller::latency`] figure): the served pipeline's chunk, which it
    /// ships to another process and plays the one before. `0`, the default,
    /// for a Processor that renders in place. Read from the node's `prepare`,
    /// after [`retune`](Self::retune), so a graph compiled from the node's
    /// shape compensates for it.
    fn latency(&self) -> Samples {
        Samples(0)
    }

    /// Follows the graph's rate, block ceiling and device quantum without a
    /// restart, for a Processor that can (the served pipeline: it tells its
    /// server the rate by a queued command and settles its chunk from the
    /// block and quantum). `true` when it took them, so the graph node does
    /// not deactivate and activate the plugin for them; `false`, the default,
    /// when only a [`Lifecycle`](crate::Lifecycle) reconfiguration can.
    ///
    /// Called from the node's `prepare`, which runs off the audio thread with
    /// the node out of any executor, so it may allocate; it starts the
    /// Processor over (its chunk phase and any chunk in flight), as a
    /// [`reset`](Self::reset) does. Needs no [`MainThread`],
    /// which is why the served hosting can take it from any thread.
    fn retune(&mut self, rate: SampleRate, max_block: Samples, quantum: Option<Samples>) -> bool {
        let _ = (rate, max_block, quantum);
        false
    }
}

/// The main-thread side of a plugin that is rendering.
///
/// `Send` so a server can build it on one thread and run it on another, not
/// `Sync`, and never used from two threads at once.
pub trait Controller: Control + Send {
    /// The loaded plugin this rejoins into.
    type Loaded: Loaded<Controller = Self, Processor = Self::Processor>;
    /// The realtime half of the same activation.
    type Processor: Processor;

    /// The latency the plugin reports now.
    fn latency(&self, main: &MainThread) -> Samples;

    /// The tail the plugin reports now.
    fn tail(&self, main: &MainThread) -> PluginTail;
}

/// A [`Control`] that is a loaded plugin's own, with what an activation adds.
///
/// An in-process format's [`Controller`] holds the [`Loaded`] it was split from
/// and answers the whole control surface through it. What an activation adds is
/// one concern: the Controller owns the pipeline of edits to and from the
/// Processor (an [`EditQueue`](crate::EditQueue) for host writes, a drain for
/// what the plugin reported, the ranges the Processor denormalizes against).
/// This trait names the plugin and the few points where that pipeline joins the
/// plugin's answers; [`Control`] and [`EditorWindow`] are implemented once for
/// every `Wraps`, here, so a format writes only those points. Every point
/// defaults to nothing, so a format with no such pipeline (VST2) writes none.
///
/// A served controller holds no plugin (its plugin is across the wire) and does
/// not implement this: it implements [`Control`] itself.
pub trait Wraps {
    /// The plugin whose surface this one answers.
    type Plugin: Control;

    /// Whether a host write reaches the plugin itself. `false` where the
    /// activated plugin must not be written from the main thread (CLAP's
    /// `params.flush` belongs to the audio thread), so the write only goes to
    /// the Processor ([`queue_write`](Self::queue_write)).
    const WRITES_PLUGIN: bool = true;

    /// Whether the render mode can change while active. `false` where the mode
    /// is fixed at activation (AU's is written before `AudioUnitInitialize`):
    /// the caller reconfigures through the `Lifecycle` and the answer is
    /// `false`.
    const SWITCHES_RENDER_MODE: bool = true;

    /// The plugin.
    fn plugin(&self) -> &Self::Plugin;

    /// The plugin, mutably.
    fn plugin_mut(&mut self) -> &mut Self::Plugin;

    /// A host write, after the plugin took it (or instead of it): hands it to
    /// the Processor.
    fn queue_write(&mut self, main: &MainThread, id: ParamId, value: Normalized) {
        let _ = (main, id, value);
    }

    /// A state or a preset load took: the writes queued before it are stale, and
    /// whatever it carried is owed to the Processor.
    fn after_restore(&mut self, main: &MainThread) {
        let _ = main;
    }

    /// The state, with the writes the Processor has not surely taken yet beside
    /// it. Defaults to the plugin's own.
    ///
    /// # Errors
    ///
    /// As [`Control::save_state`].
    fn save_with_pending(&mut self, main: &MainThread) -> PluginResult<Vec<u8>> {
        self.plugin_mut().save_state(main)
    }

    /// Appends what the Processor reported to `out`, after the plugin's own,
    /// which start at `from`.
    fn collect_reports(&mut self, main: &MainThread, out: &mut Vec<ParamEdit>, from: usize) {
        let _ = (main, out, from);
    }

    /// The reports the Controller's own pipeline lost, beside the plugin's.
    fn lost_reports(&self) -> u64 {
        0
    }

    /// A rescan changed what the parameters are: the ranges the Processor
    /// denormalizes against are read again and republished.
    fn republish_ranges(&mut self, main: &MainThread) {
        let _ = main;
    }

    /// The control-side tick (every [`Event::Tick`](crate::Event::Tick)): retry
    /// or apply what the Processor owes, and let retired published values go.
    fn tick(&mut self, main: &MainThread) {
        let _ = main;
    }
}

impl<W: Wraps> Control for W {
    fn info(&self) -> &PluginDescriptor {
        self.plugin().info()
    }

    fn wiring(&self, main: &MainThread) -> LoadedPlugin {
        self.plugin().wiring(main)
    }

    fn parameters(&self, main: &MainThread) -> Vec<ParameterInfo> {
        self.plugin().parameters(main)
    }

    fn parameter_count(&self, main: &MainThread) -> usize {
        self.plugin().parameter_count(main)
    }

    fn parameter(&self, main: &MainThread, id: ParamId) -> Option<Normalized> {
        self.plugin().parameter(main, id)
    }

    fn set_parameter(&mut self, main: &MainThread, id: ParamId, value: Normalized) {
        if W::WRITES_PLUGIN {
            self.plugin_mut().set_parameter(main, id, value);
        }
        self.queue_write(main, id, value);
    }

    fn parameter_plain(&self, main: &MainThread, id: ParamId, value: Normalized) -> Option<f64> {
        self.plugin().parameter_plain(main, id, value)
    }

    fn parameter_normalized(
        &self,
        main: &MainThread,
        id: ParamId,
        plain: f64,
    ) -> Option<Normalized> {
        self.plugin().parameter_normalized(main, id, plain)
    }

    fn parameter_text(&self, main: &MainThread, id: ParamId, value: Normalized) -> Option<String> {
        self.plugin().parameter_text(main, id, value)
    }

    fn parameter_value_from_text(
        &self,
        main: &MainThread,
        id: ParamId,
        text: &str,
    ) -> Option<Normalized> {
        self.plugin().parameter_value_from_text(main, id, text)
    }

    fn save_state(&mut self, main: &MainThread) -> PluginResult<Vec<u8>> {
        self.save_with_pending(main)
    }

    fn load_state(&mut self, main: &MainThread, bytes: &[u8]) -> PluginResult<()> {
        let result = self.plugin_mut().load_state(main, bytes);
        self.after_restore(main);
        result
    }

    fn presets(&mut self, main: &MainThread) -> Vec<Preset> {
        self.plugin_mut().presets(main)
    }

    fn load_preset(&mut self, main: &MainThread, id: &PresetId) -> bool {
        let loaded = self.plugin_mut().load_preset(main, id);
        if loaded {
            self.after_restore(main);
        }
        loaded
    }

    fn current_preset(&mut self, main: &MainThread) -> Option<PresetId> {
        self.plugin_mut().current_preset(main)
    }

    fn set_automation_state(&mut self, main: &MainThread, mode: AutomationMode) {
        self.plugin_mut().set_automation_state(main, mode);
    }

    fn rescan_parameters(&mut self, main: &MainThread, scope: RescanScope) {
        self.plugin_mut().rescan_parameters(main, scope);
        if matches!(scope, RescanScope::Info | RescanScope::All) {
            self.republish_ranges(main);
        }
    }

    fn rebuild_midi_mapping(&mut self, main: &MainThread) {
        self.plugin_mut().rebuild_midi_mapping(main);
    }

    fn flush_edits(&mut self, main: &MainThread) {
        self.plugin_mut().flush_edits(main);
        self.tick(main);
    }

    fn run_main_thread(&mut self, main: &MainThread) {
        self.plugin_mut().run_main_thread(main);
    }

    fn run_timer(&mut self, main: &MainThread, id: TimerId) {
        self.plugin_mut().run_timer(main, id);
    }

    fn service_fd(&mut self, main: &MainThread, id: FdId) {
        self.plugin_mut().service_fd(main, id);
    }

    fn take_requests(&mut self, main: &MainThread, out: &mut Vec<Request>) {
        self.plugin_mut().take_requests(main, out);
    }

    fn set_render_mode(&mut self, main: &MainThread, mode: RenderMode) -> bool {
        W::SWITCHES_RENDER_MODE && self.plugin_mut().set_render_mode(main, mode)
    }

    fn take_reported_edits(&mut self, main: &MainThread, out: &mut Vec<ParamEdit>) {
        let from = out.len();
        self.plugin_mut().take_reported_edits(main, out);
        self.collect_reports(main, out, from);
    }

    fn dropped_reported_edits(&self) -> u64 {
        self.plugin().dropped_reported_edits() + self.lost_reports()
    }
}

impl<W: Wraps> EditorWindow for W {
    fn has_editor(&self, main: &MainThread) -> bool {
        self.plugin().has_editor(main)
    }

    fn open_editor(
        &mut self,
        main: &MainThread,
        parent: WindowHandle,
        scale: ContentScale,
    ) -> PluginResult<EditorSize> {
        self.plugin_mut().open_editor(main, parent, scale)
    }

    fn open_floating_editor(&mut self, main: &MainThread) -> PluginResult<()> {
        self.plugin_mut().open_floating_editor(main)
    }

    fn close_editor(&mut self, main: &MainThread) {
        self.plugin_mut().close_editor(main);
    }

    fn editor_idle(&mut self, main: &MainThread) {
        self.plugin_mut().editor_idle(main);
    }

    fn editor_capabilities(&mut self, main: &MainThread) -> EditorCapabilities {
        self.plugin_mut().editor_capabilities(main)
    }

    fn set_editor_size(
        &mut self,
        main: &MainThread,
        requested: EditorSize,
    ) -> PluginResult<EditorSize> {
        self.plugin_mut().set_editor_size(main, requested)
    }

    fn poll_editor_resize_request(&mut self, main: &MainThread) -> Option<EditorSize> {
        self.plugin_mut().poll_editor_resize_request(main)
    }
}

/// An activation or rebuild that did not take, and the plugin it left.
#[derive(Debug)]
pub struct Refused<L> {
    /// The plugin, loaded. `None` only where a format cannot hand it back (AU,
    /// when both the activation and its compensating uninitialize fail);
    /// [`Lifecycle`](crate::Lifecycle) then poisons.
    pub plugin: Option<L>,
    /// Why.
    pub reason: PluginError,
}

/// A deactivation that did not take: both halves are as they were.
#[derive(Debug)]
pub struct Stuck<P, C> {
    /// The realtime half.
    pub processor: P,
    /// The main-thread half.
    pub controller: C,
    /// Why.
    pub reason: PluginError,
}

/// One block's buffers, inputs and outputs.
#[derive(Debug)]
pub struct PluginIo<'a> {
    /// The channel tables, in the width the plugin was activated at.
    pub buffer: AudioBufferMut<'a, 'a>,
    /// The block's MIDI, transport and automation.
    pub ctx: &'a ProcessContext<'a>,
    /// What the plugin emitted: MIDI, parameter changes, note expression.
    pub out: &'a mut ProcessOutput,
}
