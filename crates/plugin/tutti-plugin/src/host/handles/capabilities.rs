//! Granular host-side plugin-control capability traits.
//!
//! These replace the former `ControlBackend` god-trait: instead of one ~13-method
//! trait every backend implemented in full (faking the capabilities it lacked with
//! no-op/error stubs), a backend implements exactly the capabilities it honors, and
//! a consumer depends only on the capability it uses.
//!
//! **Naming.** These are the *host-side* mirror of the loader-side `Plugin*`
//! capability traits in `tutti-plugin-types` (`PluginParams`/`PluginState`/
//! `PluginEditorHost`), which live *inside the subprocess*. The two rows are named
//! apart because they are different objects doing the same job on opposite sides of
//! the IPC boundary — exactly as the host-side `AudioUnit` node mirrors the
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

use crate::error::EditorError;
use crate::protocol::{AutomationMode, ParamAddress, ParameterInfo, Preset, PresetId, RenderMode};
use crate::util::window::{EditorCapabilities, EditorSize};

/// Parameter catalog, live-value read, and imperative value write — plus the
/// backend's liveness (`is_crashed`), folded here rather than in a one-method
/// trait of its own.
///
/// Method names say *what kind of thing* each deals in: `_descriptors` is the
/// static metadata catalog; `_value` / `set_..._value` is the live number. This
/// is deliberately NOT the automation/modulation path — sample-accurate parameter
/// automation is a per-block `BlockInput` producer installed on the audio node
/// (`set_param_automation_source`), never a method here.
pub trait HostParams: Send + Sync {
    /// The static parameter catalog (id, name, range, flags). `None` if the
    /// backend cannot enumerate parameters.
    fn parameter_descriptors(&self) -> Option<Vec<ParameterInfo>>;

    /// The plugin's current live value for one parameter. `None` if unavailable
    /// — including when `id` uses the other addressing model, which addresses
    /// no parameter of this backend. See [`ParamAddress`].
    fn parameter_value(&self, id: ParamAddress) -> Option<f32>;

    /// Write one parameter value (a UI knob poke / initial preset value).
    /// Main-thread; fire-and-forget. In-process backends `try_lock` internally so
    /// a shared handle can never block the audio thread.
    fn set_parameter_value(&self, id: ParamAddress, value: f32);

    /// `true` if the underlying plugin is gone (subprocess crashed). In-process
    /// backends never return `true` — a crash takes the host down with it.
    fn is_crashed(&self) -> bool;

    /// Why the plugin died, when the backend can say.
    ///
    /// Defaulted to `None` so a backend that only tracks the bool keeps
    /// compiling: an out-of-crate in-process loader implements this trait, and
    /// a required method would have broken it for a fact it cannot report
    /// anyway (an in-process crash takes the host down with it).
    ///
    /// `Some` only when [`is_crashed`](Self::is_crashed) is `true`. The
    /// subprocess backend latches the reason at the detection site, so this
    /// answers even for a crash that happened before the host installed a
    /// listener.
    fn crash_cause(&self) -> Option<String> {
        None
    }
}

/// Opaque preset-chunk save / load. Raw `Vec<u8>` — the bytes are the plugin's
/// business (the same shape the loader-side `PluginState::get_state` returns).
pub trait HostState: Send + Sync {
    fn save_state(&self) -> Option<Vec<u8>>;
    fn load_state(&self, data: &[u8]);
}

/// Editor / GUI hosting — **optional**. A backend implements this only if it can
/// embed the plugin's editor; a headless one leaves it unimplemented, so its
/// plugins report `PluginHandle::editor() == None` rather than erroring at open
/// time.
///
/// `parent` is a raw platform window pointer (not a generic `HasWindowHandle`) so
/// the trait stays object-safe; the ergonomic `HasWindowHandle` entry lives on
/// [`PluginHandle::open_editor`](super::control_handle::PluginHandle::open_editor).
pub trait HostEditor: Send + Sync {
    fn open_editor(&self, parent: *mut c_void) -> Result<EditorSize, EditorError>;

    /// Open the editor as a **floating** window the plugin creates and owns.
    ///
    /// Takes no parent: that is the whole difference from
    /// [`open_editor`](Self::open_editor), and returns no size because the host
    /// does not lay out a window it did not create.
    ///
    /// Defaulted to a refusal rather than left abstract. Only CLAP has the
    /// concept — VST3, VST2 and AU embed unconditionally — so a default keeps
    /// three formats from carrying an override that could only say this. A
    /// caller checks [`Features::EDITOR_FLOATING`](crate::protocol::Features)
    /// before reaching here; the error is for one that did not.
    fn open_floating_editor(&self) -> Result<(), EditorError> {
        Err(EditorError::PluginError(
            "this plugin format has no floating-window editor".into(),
        ))
    }

    fn close_editor(&self);

    /// Call periodically (~30 Hz) while the editor is open.
    fn editor_idle(&self);

    fn editor_capabilities(&self) -> EditorCapabilities {
        EditorCapabilities::default()
    }

    /// Returns the snapped/clamped size the plugin actually applied.
    fn set_editor_size(&self, _requested: EditorSize) -> Result<EditorSize, EditorError> {
        Err(EditorError::PluginError(
            "set_editor_size not supported".into(),
        ))
    }

    fn poll_editor_resize_request(&self) -> Option<EditorSize> {
        None
    }
}

/// Host → plugin automation-state advisory — **optional**, Direction C-in.
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
    /// Announce the host's current automation mode to the plugin. **Global** (no
    /// `param_id`) — the only wired sink (VST3 `IAutomationState`) is global.
    ///
    /// Fallible: the call can fail because the backend is gone, there is no open
    /// editor to deliver the feedback to, or the format's automation interface is
    /// absent. `Ok(())` means *delivered / accepted*, not that the plugin visibly
    /// reacted (no format confirms the reaction).
    fn set_automation_mode(&self, mode: AutomationMode) -> Result<(), EditorError>;
}

/// Host → plugin render-mode advisory — **optional**, Direction C-in.
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
    /// Tell the plugin whether it is rendering offline.
    ///
    /// Returns whether the plugin *accepted* the mode. `false` is a refusal, not
    /// an error: a CLAP plugin that does not implement `clap.render` renders
    /// identically either way, which is exactly what declining the extension
    /// means. See [`Features::RENDER_MODE`](crate::protocol::Features).
    fn set_render_mode(&self, mode: RenderMode) -> bool;
}

/// Preset enumeration and loading — **optional**.
///
/// Two methods rather than one because **no format has both unconditionally**,
/// and the pair a format answers differs:
///
/// - **CLAP** loads by filesystem path but cannot enumerate — discovery is a
///   factory-level extension this host does not bind.
/// - **VST3** enumerates richly but has no load call: a program is selected by
///   writing the parameter flagged `kIsProgramChange`, through the ordinary
///   parameter path.
/// - **AU** and **VST2** answer both.
///
/// That asymmetry is why [`Features::PRESET_LIST`] and
/// [`Features::PRESET_LOAD`] are two bits, and why this is not one
/// `presets_supported() -> bool`.
///
/// A backend implements this only if it can reach presets at all; others do
/// not, so [`PluginHandle::presets`] returns `None` — no stub.
///
/// [`Features::PRESET_LIST`]: crate::protocol::Features::PRESET_LIST
/// [`Features::PRESET_LOAD`]: crate::protocol::Features::PRESET_LOAD
/// [`PluginHandle::presets`]: super::control_handle::PluginHandle::presets
pub trait HostPresets: Send + Sync {
    /// Every preset the plugin advertises, in the plugin's own order.
    ///
    /// Empty when the format cannot enumerate (CLAP), which is **not** the same
    /// as a plugin with no presets. A caller distinguishing the two reads
    /// [`Features::PRESET_LIST`] on `loaded()`, exactly as it would for any
    /// other unasked-versus-declined capability.
    ///
    /// [`Features::PRESET_LIST`]: crate::protocol::Features::PRESET_LIST
    fn presets(&self) -> Vec<Preset>;

    /// Ask the plugin to load one, by an id [`presets`](Self::presets) produced.
    ///
    /// Returns whether the plugin *accepted*. `false` is a refusal, not an
    /// error — and it is also the honest answer for VST3, whose programs are
    /// reached through the parameter path rather than a load call. Routing
    /// program selection through here as well would give one operation two
    /// write paths.
    ///
    /// The result is returned rather than swallowed for the reason
    /// [`HostRenderMode::set_render_mode`] gives: a refusal changes what the
    /// caller must do next — leave the UI selection where it was, rather than
    /// move it to a preset the plugin never loaded.
    fn load_preset(&self, id: &PresetId) -> bool;

    /// Which preset the plugin considers current, when it will say.
    ///
    /// `None` means the format has no query for it (VST3, CLAP) or the plugin
    /// declined — never "the first one". A caller must not substitute an index
    /// of its own; see [`PresetId`], where three of four formats number in a
    /// space that is not a position.
    fn current_preset(&self) -> Option<PresetId>;
}

/// Compile-time guard that all six control capabilities stay **object-safe** —
/// `PluginHandle` stores each as `Arc<dyn …>`, so a regression that breaks
/// dyn-compatibility (e.g. adding a generic method) must fail here, not at a
/// distant call site.
#[allow(dead_code)]
fn _assert_object_safe(
    _p: &dyn HostParams,
    _s: &dyn HostState,
    _e: &dyn HostEditor,
    _a: &dyn HostAutomationState,
    _r: &dyn HostRenderMode,
    _pr: &dyn HostPresets,
) {
}
