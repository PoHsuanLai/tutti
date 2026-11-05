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
//! at a time. [`Processor::process`] and [`Processor::stop`] run on whichever
//! thread holds the Processor and are the only calls that thread makes. An
//! implementation's `unsafe` states, per format, why the two halves may reach
//! one plugin instance at once.

use crate::{
    AudioBufferMut, AutomationMode, LoadedPlugin, Normalized, ParamId, ParameterInfo,
    PluginDescriptor, PluginError, PluginResult, PluginTail, Preset, PresetId, ProcessContext,
    ProcessOutput, Samples,
};
use crate::{FdId, Prepare, RescanScope, RestartRules, TimerId};

/// What the main thread asks of a plugin, in any state that has one.
///
/// Object-safe: [`Lifecycle::control`](crate::Lifecycle::control) lends it as
/// `&mut dyn Control`.
pub trait Control {
    /// The plugin's catalog identity.
    fn info(&self) -> &PluginDescriptor;

    /// The wiring read from the plugin now, not a load-time cache.
    fn wiring(&self) -> LoadedPlugin;

    /// Every parameter the plugin declares.
    fn parameters(&self) -> Vec<ParameterInfo>;

    /// The parameter's current value, or `None` if the plugin has no such
    /// parameter.
    fn parameter(&self, id: ParamId) -> Option<Normalized>;

    /// Sets a parameter. Loaded: applied now. Active: an edit queued to the
    /// Processor, applied at the head of its next block
    /// ([`Edit`](crate::Edit)).
    fn set_parameter(&mut self, id: ParamId, value: Normalized);

    /// The plugin's display text for `value`, if it has any.
    fn parameter_text(&self, id: ParamId, value: Normalized) -> Option<String> {
        let _ = (id, value);
        None
    }

    /// The value the plugin parses from `text`, if it can.
    fn parameter_value_from_text(&self, id: ParamId, text: &str) -> Option<Normalized> {
        let _ = (id, text);
        None
    }

    /// The plugin's state as bytes.
    ///
    /// # Errors
    ///
    /// [`PluginError::State`] when the plugin cannot save.
    fn save_state(&mut self) -> PluginResult<Vec<u8>>;

    /// Restores state saved by [`save_state`](Self::save_state).
    ///
    /// # Errors
    ///
    /// [`PluginError::State`] when the plugin rejects the bytes.
    fn load_state(&mut self, bytes: &[u8]) -> PluginResult<()>;

    /// The plugin's factory presets. Empty when the format has none.
    fn presets(&mut self) -> Vec<Preset> {
        Vec::new()
    }

    /// Loads a preset; `false` when the format or the plugin cannot.
    fn load_preset(&mut self, id: &PresetId) -> bool {
        let _ = id;
        false
    }

    /// The preset the plugin is on, if it says.
    fn current_preset(&mut self) -> Option<PresetId> {
        None
    }

    /// Tells the plugin how the host is automating it.
    fn set_automation_state(&mut self, mode: AutomationMode) {
        let _ = mode;
    }

    /// Reads parameters again after a rescan the plugin asked for.
    fn rescan_parameters(&mut self, scope: RescanScope) {
        let _ = scope;
    }

    /// The plugin's own main-thread work (CLAP `on_main_thread`).
    fn run_main_thread(&mut self) {}

    /// Runs a plugin timer that is due.
    fn run_timer(&mut self, id: TimerId) {
        let _ = id;
    }

    /// Services a plugin file descriptor that is ready.
    fn service_fd(&mut self, id: FdId) {
        let _ = id;
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
    fn activate(self, prepare: Prepare)
    -> Result<(Self::Processor, Self::Controller), Refused<Self>>;

    /// Rejoins the halves of one activation.
    ///
    /// # Errors
    ///
    /// [`Stuck`], with both halves, for a pair from two activations or a plugin
    /// that will not stop.
    fn deactivate(
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
    fn rebuild(self) -> Result<Self, Refused<Self>> {
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
    /// # Errors
    ///
    /// [`PluginError::Process`] when the plugin failed the block.
    fn process(&mut self, io: &mut PluginIo<'_>) -> PluginResult<()>;

    /// Ends processing (CLAP `stop_processing`, VST3 `setProcessing(false)`) so
    /// the realtime thread can hand the Processor back. Realtime. Processing
    /// starts again lazily in the first [`process`](Self::process).
    fn stop(&mut self);
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
    fn latency(&self) -> Samples;

    /// The tail the plugin reports now.
    fn tail(&self) -> PluginTail;
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
