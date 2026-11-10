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

use crate::{
    AudioBufferMut, AutomationMode, EditorWindow, LoadedPlugin, Normalized, ParamId, ParameterInfo,
    PluginDescriptor, PluginError, PluginResult, PluginTail, Preset, PresetId, ProcessContext,
    ProcessOutput, Samples,
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

    /// The parameter's current value, or `None` if the plugin has no such
    /// parameter.
    fn parameter(&self, main: &MainThread, id: ParamId) -> Option<Normalized>;

    /// Sets a parameter. Loaded: applied now. Active: an edit queued to the
    /// Processor, applied at the head of its next block
    /// ([`Edit`](crate::Edit)).
    fn set_parameter(&mut self, main: &MainThread, id: ParamId, value: Normalized);

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

    /// Appends the parameter edits the plugin reported outside a block (its own
    /// editor moving a knob), oldest first, to `out`. Only VST2 reports them
    /// this way; CLAP and VST3 report them in the block's [`ProcessOutput`] and
    /// push nothing here.
    fn take_reported_edits(&mut self, main: &MainThread, out: &mut Vec<(ParamId, Normalized)>) {
        let _ = (main, out);
    }
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
    /// request a plugin may ignore); VST2 has no opcode that clears state, so
    /// only the host's own SysEx reassembly is dropped. Every format drops a
    /// SysEx open across the reset (design 016 §6.2).
    fn reset(&mut self);
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
