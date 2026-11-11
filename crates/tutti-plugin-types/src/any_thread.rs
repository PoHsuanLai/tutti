//! What a loaded plugin answers from any thread ([`AnyThread`]), and its
//! liveness as its host observes it ([`Health`]).
//!
//! Both are what a `PluginHandle` holds beside the main-thread
//! [`Control`](crate::Control): a handle is cloned into pool jobs and
//! polled from any thread, so these calls take no
//! [`MainThread`](crate::MainThread).

use crate::{
    AutomationMode, BlockCounts, EditorError, Normalized, ParamId, ParameterInfo, RenderMode,
    SampleRate, StateError,
};

/// What a loaded plugin answers from any thread, without a main-thread token.
///
/// [`Control`](crate::Control) is a format's main-thread surface, and every
/// call of it takes the token that proves it. This is a *hosting's* surface,
/// and the two overlap on purpose, because the two hostings differ in how they
/// reach the plugin from a thread that is not the main one:
///
/// - **In-process**, the plugin may only be touched on the main thread, so the
///   hosting hops there (`defer_to_main`) and calls `Control`; off the main
///   thread the call waits for the next main-thread tick up to a deadline, and
///   answers "unavailable" when it passes (`None`, `false`,
///   [`StateError::Unavailable`], or the write is dropped).
/// - **Served**, a bridge answers from any thread, concurrently with the main
///   thread's own calls, so no hop is added (design 021 §10.4 (d)). A host
///   that blocks its main thread on a snapshot or a parameter read would be
///   told "unavailable" if it hopped, for a call the bridge can answer.
///
/// A token-free `Control` would erase that difference by pretending the main
/// thread is not needed, which is false for every in-process format; so the
/// token stays on `Control` and the calls a handle makes without one live here.
///
/// Object-safe, `&self`, `Send + Sync`: a handle stores it as `Arc<dyn
/// AnyThread>`.
pub trait AnyThread: Send + Sync {
    /// The static parameter catalog (id, name, range, flags), or `None` when
    /// it is unavailable: the plugin is lost, or (in-process, off the main
    /// thread) the main thread did not answer in time.
    fn parameter_descriptors(&self) -> Option<Vec<ParameterInfo>>;

    /// The plugin's current value for one parameter.
    ///
    /// `None` if unavailable, including when `id` uses the other addressing
    /// model, which addresses no parameter of this plugin. See [`ParamId`].
    fn parameter_value(&self, id: ParamId) -> Option<Normalized>;

    /// Writes one parameter value, for a UI knob or an initial value.
    /// Fire-and-forget, and never blocks the audio thread.
    ///
    /// [`Normalized`], not a bare float: an in-process plugin is written
    /// straight through, so the type is what stops an out-of-range or NaN
    /// value there (a served one is clamped again on receipt).
    fn set_parameter_value(&self, id: ParamId, value: Normalized);

    /// The plugin's own display string for `value`, such as `"800 Hz"`.
    ///
    /// `None` means the plugin did not answer and the caller renders the number
    /// itself; that is not the same as an empty label.
    fn parameter_text(&self, id: ParamId, value: Normalized) -> Option<String>;

    /// Parses `text` with the plugin's own interpretation, for a user typing
    /// into a parameter field. `None` when it cannot, and the caller then
    /// leaves the field where it was.
    ///
    /// **Not a pure query on every format.** VST2's `effString2Parameter` is
    /// a setter with no parse-only counterpart, so asking applies the value.
    fn parameter_value_from_text(&self, id: ParamId, text: &str) -> Option<Normalized>;

    /// Serializes the plugin's state: its own bytes, the same a project save
    /// stores. A plugin with nothing to save answers `Ok(vec![])`.
    ///
    /// Blocks on a round trip (served) or a main-thread tick (in-process).
    ///
    /// # Errors
    ///
    /// A [`StateError`] saying why no state was produced: the plugin crashed,
    /// refused, the state exceeds the transport's limit, the transfer stalled,
    /// or ([`StateError::Unavailable`]) the plugin was not asked because the
    /// main thread did not answer in time.
    fn save_state(&self) -> Result<Vec<u8>, StateError>;

    /// Tells the plugin whether it is rendering offline, and returns whether it
    /// *accepted* the mode (`false` is a refusal or an unavailable plugin, not
    /// an error: the render is unchanged). See
    /// [`Features::RENDER_MODE`](crate::Features).
    fn set_render_mode(&self, mode: RenderMode) -> bool;

    /// Announces the host's automation mode, for the editor's UI feedback
    /// (VST3 `IAutomationState`). Applies to the whole plugin. `Ok(())` means
    /// delivered, not that the plugin visibly reacted.
    ///
    /// # Errors
    ///
    /// An [`EditorError`] if the plugin is gone, the push was dropped (the
    /// command queue was full), or this hosting carries no such advisory
    /// ([`EditorError::GuiNotSupported`]).
    fn set_automation_mode(&self, mode: AutomationMode) -> Result<(), EditorError>;
}

/// The plugin's liveness as its host observes it.
///
/// What a `PluginHandle` reports as its `status`
/// and acts on: a served plugin can die, refuse a rate, or stop answering, and
/// an in-process one cannot be observed to (a crash takes the host with it), so
/// its answers say so (never crashed, no refusal, zero counts, nothing to end). Atomic reads and one kill, callable from any
/// thread.
pub trait Health: Send + Sync {
    /// `true` if the plugin is gone (the subprocess crashed). An in-process
    /// plugin is never observed `true`.
    fn is_crashed(&self) -> bool;

    /// Why the plugin died, when it can say. `Some` only when
    /// [`is_crashed`](Self::is_crashed) is `true`. Latched at the detection
    /// site, so it answers even for a crash before the host listened.
    fn crash_cause(&self) -> Option<String>;

    /// The sample rate the plugin refused and its reason, while it is refused.
    /// An in-process plugin cannot refuse.
    fn rate_refusal(&self) -> Option<(SampleRate, String)>;

    /// Blocks the audio path gave up waiting for, and replies it took (on time
    /// or late), over the plugin's life: a plugin that stays alive but never
    /// returns from `process()` shows as the first growing while the second
    /// does not. An in-process plugin has no budget to miss.
    fn block_counts(&self) -> BlockCounts;

    /// Ends the plugin's server process, latching `cause` as why it is dead
    /// (unless it already died of something else). An in-process
    /// plugin has no process to end.
    fn terminate(&self, cause: &str);
}
