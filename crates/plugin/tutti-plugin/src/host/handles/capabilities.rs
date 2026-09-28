//! Granular host-side plugin-control capability traits.
//!
//! A backend implements exactly the capabilities it honors, and a consumer
//! depends only on the capability it uses; there are no no-op or error stubs
//! for missing capabilities.
//!
//! **Naming.** These are the *host-side* mirror of the loader-side `Plugin*`
//! capability traits in `tutti-plugin-types` (`PluginParams`/`PluginState`/
//! `PluginEditorHost`), which live *inside the subprocess*. The two rows are named
//! apart because they are different objects doing the same job on opposite sides of
//! the IPC boundary — exactly as the host-side graph node mirrors the
//! loader-side `PluginAudio`. So the host-side control traits take the `Host*`
//! prefix.
//!
//! All are object-safe (`&self`, no generics on the trait methods, `Send + Sync`):
//! a `PluginHandle` stores them as `Arc<dyn …>`.
//!
//! - [`HostParams`] and [`HostState`] are **always present** — every backend
//!   implements them.
//! - [`HostEditor`] is **optional**: a backend with no embeddable editor simply
//!   does not implement it, so `PluginHandle::editor()` returns `None`. Absence
//!   is type-level; there is no `open_editor → Err(GuiNotSupported)` stub to
//!   fake it.
//! - `is_crashed` is a single-method concern folded onto the always-present set via
//!   [`HostParams`], rather than a standalone trait too thin to stand alone.

use std::ffi::c_void;

use crate::error::{EditorError, StateError};
use crate::protocol::{
    AutomationMode, Normalized, ParamAddress, ParameterInfo, Preset, PresetId, RenderMode,
};
use crate::util::window::{EditorCapabilities, EditorSize};

/// Parameter catalog, live-value reads and writes, and the backend's liveness.
///
/// Every backend implements this; [`PluginHandle::params`](super::control_handle::PluginHandle::params)
/// returns it. `is_crashed` lives here rather than in a one-method trait of its
/// own.
///
/// Method names say *what kind of thing* each deals in: `_descriptors` is the
/// static metadata catalog; `_value` / `set_..._value` is the live number. This
/// is deliberately NOT the automation/modulation path — sample-accurate parameter
/// automation is an event source node wired to the plugin node
/// (`PluginControls::automation`), never a method here.
pub trait HostParams: Send + Sync {
    /// Returns the static parameter catalog (id, name, range, flags), or
    /// `None` if the backend cannot enumerate parameters.
    fn parameter_descriptors(&self) -> Option<Vec<ParameterInfo>>;

    /// Returns the plugin's current value for one parameter.
    ///
    /// `None` if unavailable, including when `id` uses the other addressing
    /// model, which addresses no parameter of this backend. See
    /// [`ParamAddress`].
    fn parameter_value(&self, id: ParamAddress) -> Option<f32>;

    /// Writes one parameter value, for a UI knob or an initial value.
    ///
    /// Main thread; fire-and-forget. In-process backends `try_lock` internally
    /// so a shared handle can never block the audio thread.
    ///
    /// [`Normalized`], not a bare float, because this is the *front door*: a
    /// caller here holds a [`ParameterInfo`] and can see a `[10, 22050]` Hz
    /// range on it, which is exactly what invites writing `20_000.0` and
    /// getting full scale. Denormalizing against the declared range is the
    /// backend's job, discharged where the range is known — see
    /// [`PluginParams::set_parameter`](tutti_plugin_types::PluginParams::set_parameter)
    /// for the same argument one layer down.
    ///
    /// The subprocess path clamps again on receipt, but in-process backends
    /// write straight through to the plugin, so [`Normalized`] is what stops an
    /// out-of-range or NaN value there.
    fn set_parameter_value(&self, id: ParamAddress, value: Normalized);

    /// Returns the plugin's own display string for `value`, such as `"800 Hz"`
    /// or `"Bandpass"`.
    ///
    /// `None` means the plugin did not answer, and the caller should render the
    /// number itself. That is not the same as an empty label, which is why this
    /// is an `Option<String>` rather than a `String` defaulting to `""`.
    ///
    /// Defaults to `None`, so a backend whose format cannot answer need not
    /// implement it.
    ///
    /// See
    /// [`PluginParams::parameter_text`](tutti_plugin_types::PluginParams::parameter_text)
    /// for the domain rule — `value` is normalized here and each loader
    /// converts at its own edge.
    fn parameter_text(&self, id: ParamAddress, value: Normalized) -> Option<String> {
        let _ = (id, value);
        None
    }

    /// Parses `text` with the plugin's own interpretation, for a user typing
    /// into a parameter field.
    ///
    /// `None` when it cannot parse the string; the caller must then leave the
    /// field where it was, since a fabricated value would be committed to the
    /// user's preset.
    ///
    /// **Not a pure query on every format.** VST2's `effString2Parameter` is a
    /// setter with no parse-only counterpart, so on that backend asking applies
    /// the value.
    fn parameter_value_from_text(&self, id: ParamAddress, text: &str) -> Option<Normalized> {
        let _ = (id, text);
        None
    }

    /// Returns `true` if the underlying plugin is gone (the subprocess crashed).
    ///
    /// In-process backends never return `true`: a crash takes the host down
    /// with it.
    fn is_crashed(&self) -> bool;

    /// Returns why the plugin died, when the backend can say.
    ///
    /// Defaults to `None`, so a backend that only tracks
    /// [`is_crashed`](Self::is_crashed) need not implement it.
    ///
    /// `Some` only when [`is_crashed`](Self::is_crashed) is `true`. The
    /// subprocess backend latches the reason at the detection site, so this
    /// answers even for a crash that happened before the host installed a
    /// listener.
    fn crash_cause(&self) -> Option<String> {
        None
    }
}

/// Saves and restores the plugin's state as an opaque byte blob.
///
/// Every backend implements this; the bytes are the plugin's own format, the
/// same data a project save stores. Both calls block on a subprocess round
/// trip for out-of-process plugins, so call them from a control thread.
pub trait HostState: Send + Sync {
    /// Serializes the plugin's current state.
    ///
    /// A plugin that has nothing to save answers `Ok(vec![])`.
    ///
    /// # Errors
    ///
    /// Returns a [`StateError`] saying why no state was produced: the plugin
    /// crashed, refused, the backend cannot carry state, the state exceeds the
    /// transport's size limit, or the transfer stalled.
    fn save_state(&self) -> Result<Vec<u8>, StateError>;

    /// Restores a blob produced by [`save_state`](Self::save_state).
    ///
    /// # Errors
    ///
    /// Returns a [`StateError`] if the state was not applied. A plugin
    /// declining a chunk ([`StateError::Rejected`]) is routine: it happens when
    /// a state saved by an older build is loaded into a newer one, when a file
    /// is truncated, or when a chunk from a different plugin is fed in. Report
    /// it to the user rather than leaving the plugin silently at defaults.
    fn load_state(&self, data: &[u8]) -> Result<(), StateError>;
}

/// Hosts the plugin's editor window. **Optional.**
///
/// A backend implements this only if it can show the plugin's editor; for one
/// that cannot, `PluginHandle::editor()` returns `None`. Call these on the main
/// (UI) thread.
///
/// `parent` is a raw platform window pointer (not a generic `HasWindowHandle`) so
/// the trait stays object-safe; the ergonomic `HasWindowHandle` entry lives on
/// [`PluginHandle::open_editor`](super::control_handle::PluginHandle::open_editor).
pub trait HostEditor: Send + Sync {
    /// Embeds the plugin's editor as a child of `parent`, returning the size it
    /// asked for.
    ///
    /// # Errors
    ///
    /// Returns an [`EditorError`] if the plugin has no editor or failed to
    /// create or attach it, or if the backend is gone.
    fn open_editor(&self, parent: *mut c_void) -> Result<EditorSize, EditorError>;

    /// Opens the editor as a **floating** window the plugin creates and owns.
    ///
    /// Takes no parent, unlike [`open_editor`](Self::open_editor), and returns
    /// no size because the host does not lay out a window it did not create.
    /// Only CLAP has floating editors; check
    /// [`Features::EDITOR_FLOATING`](crate::protocol::Features) first.
    ///
    /// # Errors
    ///
    /// Returns an [`EditorError`] if the format or plugin has no floating
    /// editor (the default implementation always does) or the plugin failed
    /// to open it.
    fn open_floating_editor(&self) -> Result<(), EditorError> {
        Err(EditorError::PluginError(
            "this plugin format has no floating-window editor".into(),
        ))
    }

    /// Closes the editor, whether embedded or floating.
    fn close_editor(&self);

    /// Services the editor; call periodically (about 30 Hz) while it is open.
    fn editor_idle(&self);

    /// Returns what this editor supports: resizing, DPI scaling and the rest.
    ///
    /// Defaults to [`EditorCapabilities::default`].
    fn editor_capabilities(&self) -> EditorCapabilities {
        EditorCapabilities::default()
    }

    /// Asks the plugin to resize its editor and returns the size it actually
    /// applied, which may be snapped or clamped.
    ///
    /// # Errors
    ///
    /// Returns an [`EditorError`] if the editor cannot be resized (the default
    /// implementation always does) or the plugin refused.
    fn set_editor_size(&self, _requested: EditorSize) -> Result<EditorSize, EditorError> {
        Err(EditorError::PluginError(
            "set_editor_size not supported".into(),
        ))
    }

    /// Takes a pending plugin-initiated resize request, if one is queued.
    ///
    /// Polled rather than delivered by callback: the request arrives on the
    /// plugin's own thread, and the host resizes its window on the UI thread.
    fn poll_editor_resize_request(&self) -> Option<EditorSize> {
        None
    }
}

/// Tells the plugin what the host is doing with automation. **Optional.**
///
/// The host announces what it is doing with automation (reading / writing /
/// neither) so the plugin's editor can show UI feedback (a glowing knob ring
/// while the host records automation onto it). The plugin does nothing audible
/// with this — it is purely cosmetic. A backend implements this only if the
/// underlying format supports the advisory (VST3 `IAutomationState`); others do
/// not implement it, so [`PluginHandle::automation_state`] returns `None` — no
/// stub.
///
/// [`PluginHandle::automation_state`]: super::control_handle::PluginHandle::automation_state
pub trait HostAutomationState: Send + Sync {
    /// Announces the host's current automation mode to the plugin.
    ///
    /// Applies to the whole plugin, not one parameter (VST3 `IAutomationState`
    /// is global). `Ok(())` means delivered and accepted, not that the plugin
    /// visibly reacted; no format confirms the reaction.
    ///
    /// # Errors
    ///
    /// Returns an [`EditorError`] if the backend is gone, there is no open
    /// editor to deliver the feedback to, or the plugin lacks the format's
    /// automation-state interface.
    fn set_automation_mode(&self, mode: AutomationMode) -> Result<(), EditorError>;
}

/// Tells the plugin whether it is rendering offline. **Optional.**
///
/// The host tells the plugin whether it is rendering under realtime pressure so
/// the plugin can pick a more expensive algorithm for an offline bounce. Unlike
/// [`HostAutomationState`] this is not cosmetic: it changes what the plugin
/// computes, which is why the return says whether the plugin took it.
///
/// Sits on the control surface rather than only on the audio node because the
/// caller is a bounce driver, which holds a [`PluginHandle`] and runs on the
/// control thread. Three of the four formats can only accept the change while
/// the plugin is deactivated, so it is not something an audio-thread caller
/// could deliver anyway.
///
/// A backend implements this only if it can carry the mode; others do not, so
/// [`PluginHandle::render_mode`] returns `None` — no stub.
///
/// [`PluginHandle`]: super::control_handle::PluginHandle
/// [`PluginHandle::render_mode`]: super::control_handle::PluginHandle::render_mode
pub trait HostRenderMode: Send + Sync {
    /// Tells the plugin whether it is rendering offline.
    ///
    /// Returns whether the plugin *accepted* the mode. `false` is a refusal, not
    /// an error: a CLAP plugin that does not implement `clap.render` renders
    /// identically either way, which is exactly what declining the extension
    /// means. See [`Features::RENDER_MODE`](crate::protocol::Features).
    fn set_render_mode(&self, mode: RenderMode) -> bool;
}

/// Lists and loads the plugin's presets. **Optional.**
///
/// Formats differ in which half they support, which is why
/// [`Features::PRESET_LIST`] and [`Features::PRESET_LOAD`] are two bits:
///
/// - **CLAP** loads by filesystem path but cannot enumerate (discovery is a
///   factory-level extension this host does not bind).
/// - **VST3** lists and loads the programs of a plugin that exposes a program
///   list; loading writes the parameter flagged `kIsProgramChange`.
/// - **AU** and **VST2** list and load.
///
/// Calls block on a subprocess round trip for out-of-process plugins, so make
/// them from a control thread.
///
/// A backend implements this only if it can reach presets at all; others do
/// not, so [`PluginHandle::presets`] returns `None` — no stub.
///
/// [`Features::PRESET_LIST`]: crate::protocol::Features::PRESET_LIST
/// [`Features::PRESET_LOAD`]: crate::protocol::Features::PRESET_LOAD
/// [`PluginHandle::presets`]: super::control_handle::PluginHandle::presets
pub trait HostPresets: Send + Sync {
    /// Returns every preset the plugin advertises, in the plugin's own order.
    ///
    /// Empty when the format cannot enumerate (CLAP), which is **not** the same
    /// as a plugin with no presets. A caller distinguishing the two reads
    /// [`Features::PRESET_LIST`] on `loaded()`, exactly as it would for any
    /// other unasked-versus-declined capability.
    ///
    /// [`Features::PRESET_LIST`]: crate::protocol::Features::PRESET_LIST
    fn presets(&self) -> Vec<Preset>;

    /// Asks the plugin to load a preset, by an id [`presets`](Self::presets)
    /// produced (or, for CLAP, a path id).
    ///
    /// Returns whether the plugin *accepted*. `false` is a refusal, not an
    /// error: leave the UI selection where it was rather than move it to a
    /// preset the plugin never loaded.
    fn load_preset(&self, id: &PresetId) -> bool;

    /// Returns the preset the plugin considers current, when it will say.
    ///
    /// `None` means the format has no query for it (CLAP) or the plugin
    /// declined — never "the first one". Do not substitute an index of your
    /// own; see [`PresetId`], where most formats number presets in a space that
    /// is not a position.
    fn current_preset(&self) -> Option<PresetId>;
}

/// Compile-time guard that all six control capabilities stay **object-safe** —
/// `PluginHandle` stores each as `Arc<dyn …>`, so a regression that breaks
/// dyn-compatibility (e.g. adding a generic method) must fail here, not at a
/// distant call site.
#[allow(
    dead_code,
    reason = "exists only to be type-checked — a call site would add nothing"
)]
fn _assert_object_safe(
    _p: &dyn HostParams,
    _s: &dyn HostState,
    _e: &dyn HostEditor,
    _a: &dyn HostAutomationState,
    _r: &dyn HostRenderMode,
    _pr: &dyn HostPresets,
) {
}
