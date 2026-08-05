use crate::error::EditorError;
use crate::host::handles::capabilities::{
    HostAutomationState, HostEditor, HostParams, HostPresets, HostRenderMode, HostState,
};
use crate::host::ipc_client::audio::{PluginInvalidation, PluginRefresh};
use crate::host::node::{InvalidateSink, ParameterChangeSink, RefreshSink};
use crate::protocol::AutomationMode;
use crate::protocol::{
    LoadedPlugin, ParamAddress, ParameterInfo, PluginDescriptor, Preset, PresetId,
};
use crate::util::window::{EditorCapabilities, EditorSize};
use raw_window_handle::HasWindowHandle;
use std::sync::Arc;
use tutti_midi_runtime::MidiSender;

/// The capabilities a backend may or may not honour, for
/// [`PluginHandle::from_backend`].
///
/// Each field is independent — a backend can carry the render mode without
/// hosting an editor, or reach presets without either. Default is "honours
/// none"; name the ones a backend does.
///
/// ```ignore
/// PluginHandle::from_backend(
///     backend,
///     OptionalCapabilities { editor: Some(editor), ..Default::default() },
///     descriptor, loaded, param_sink, midi_sender,
/// )
/// ```
#[derive(Default, Clone)]
pub struct OptionalCapabilities {
    /// Embeddable (and possibly floating) editor hosting.
    pub editor: Option<Arc<dyn HostEditor>>,
    /// The offline/realtime render-mode advisory.
    pub render_mode: Option<Arc<dyn HostRenderMode>>,
    /// Preset enumeration and loading.
    pub presets: Option<Arc<dyn HostPresets>>,
}

/// Main-thread control handle for a loaded plugin.
///
/// Backend-agnostic: it holds each control capability as a separate `Arc<dyn …>`
/// slot ([`HostParams`], [`HostState`], and an *optional* [`HostEditor`]), so
/// out-of-process VST3/CLAP/AU and in-process VST2 hosting share this surface
/// while advertising only the capabilities they honor. One backend object
/// implements several capability traits; construction clones the *same* backend
/// `Arc` into each always-present slot (cheap — Arc-based — and shared state stays
/// intact), and passes the optional editor slot explicitly. There is no stored
/// bundle/union trait object.
///
/// Clone is cheap (Arc-based). Action methods return `&Self` for chaining.
#[derive(Clone)]
pub struct PluginHandle {
    params: Arc<dyn HostParams>,
    state: Arc<dyn HostState>,
    editor: Option<Arc<dyn HostEditor>>,
    automation_state: Option<Arc<dyn HostAutomationState>>,
    render_mode: Option<Arc<dyn HostRenderMode>>,
    presets: Option<Arc<dyn HostPresets>>,
    descriptor: PluginDescriptor,
    loaded: LoadedPlugin,
    param_sink: ParameterChangeSink,
    refresh_sink: RefreshSink,
    invalidate_sink: InvalidateSink,
    midi_sender: MidiSender,
}

impl PluginHandle {
    /// Construct from a `PluginClient` (out-of-process backend). Call
    /// this before moving the client into the fundsp graph. The subprocess
    /// backend honors every capability, including the editor.
    pub fn from_client(client: &crate::host::node::PluginClient) -> Self {
        let backend = Arc::new(crate::host::ipc_client::SubprocessBackend::new(
            client.bridge(),
            Arc::clone(client.process_guard()),
        ));
        Self {
            params: backend.clone(),
            state: backend.clone(),
            editor: Some(backend.clone()),
            // The subprocess backend supports the automation-state advisory
            // (VST3 IAutomationState; a no-op for CLAP/AU behind the wire).
            automation_state: Some(backend.clone()),
            // Every subprocess format can carry a render mode; whether the
            // loaded plugin honours it is `Features::RENDER_MODE`, not this.
            render_mode: Some(backend.clone()),
            // Every subprocess format can reach presets; which half it can
            // actually do is `Features::PRESET_LIST` / `PRESET_LOAD`, not this.
            presets: Some(backend),
            descriptor: client.descriptor().clone(),
            loaded: client.loaded().clone(),
            param_sink: client.param_sink().clone(),
            refresh_sink: client.refresh_sink().clone(),
            invalidate_sink: client.invalidate_sink().clone(),
            midi_sender: client.midi_sender(),
        }
    }

    /// Construct from an in-process backend that implements the always-present
    /// capabilities, plus optional editor and render-mode routes. Used by every
    /// in-process loader — the in-crate VST2 path passes `Some(backend)` for
    /// both; a headless out-of-crate loader passes `None`.
    ///
    /// `backend: Arc<B>` is coerced into the `params`/`state` slots at the call
    /// site (both are clones of the same object), so shared state stays intact.
    ///
    /// The optional capabilities are a struct rather than positional parameters
    /// because they are independent — a backend may carry the render mode
    /// without hosting an editor, or the reverse — and a call site passing
    /// `None, None, None` says nothing about which slot is which. Build it with
    /// `..Default::default()` and name only what the backend honours.
    pub fn from_backend<B: HostParams + HostState + 'static>(
        backend: Arc<B>,
        optional: OptionalCapabilities,
        descriptor: PluginDescriptor,
        loaded: LoadedPlugin,
        param_sink: ParameterChangeSink,
        midi_sender: MidiSender,
    ) -> Self {
        let OptionalCapabilities {
            editor,
            render_mode,
            presets,
        } = optional;
        Self {
            params: backend.clone(),
            state: backend,
            editor,
            // In-process backends don't implement the VST3-style
            // automation-state advisory, so `automation_state()` is `None`.
            automation_state: None,
            render_mode,
            presets,
            descriptor,
            loaded,
            param_sink,
            // In-process backends have no latency-change or
            // restartComponent mechanism, so they never emit refresh /
            // invalidate signals — these sinks stay empty.
            refresh_sink: RefreshSink::default(),
            invalidate_sink: InvalidateSink::default(),
            midi_sender,
        }
    }

    #[cfg(test)]
    pub(crate) fn from_bridge_and_metadata(
        bridge: Arc<crate::host::ipc_client::PluginBridge>,
        descriptor: PluginDescriptor,
        loaded: LoadedPlugin,
    ) -> Self {
        let guard = Arc::new(crate::host::node::ProcessGuard::for_test(
            crate::util::config::BridgeConfig::default(),
        ));
        let backend = Arc::new(crate::host::ipc_client::SubprocessBackend::new(
            bridge, guard,
        ));
        let (sender, _receiver) =
            tutti_midi_runtime::MidiMailbox::pair(tutti_midi_types::MidiUnitId::next());
        Self {
            params: backend.clone(),
            state: backend.clone(),
            editor: Some(backend.clone()),
            automation_state: Some(backend.clone()),
            render_mode: Some(backend.clone()),
            presets: Some(backend),
            descriptor,
            loaded,
            param_sink: ParameterChangeSink::default(),
            refresh_sink: RefreshSink::default(),
            invalidate_sink: InvalidateSink::default(),
            midi_sender: sender,
        }
    }

    // ---- Capability accessors ---------------------------------------------

    /// The always-present parameter capability (catalog / read / write / health).
    pub fn params(&self) -> &dyn HostParams {
        self.params.as_ref()
    }

    /// The always-present state (preset save/load) capability.
    pub fn state(&self) -> &dyn HostState {
        self.state.as_ref()
    }

    /// The editor capability, or `None` when the backend cannot host an
    /// embeddable editor. The "why" is queryable separately via
    /// [`has_editor`](Self::has_editor) / the [`descriptor`](Self::descriptor).
    pub fn editor(&self) -> Option<&dyn HostEditor> {
        self.editor.as_deref()
    }

    /// The automation-state advisory capability (Direction C-in), or `None` when
    /// the backend doesn't support it (in-process VST2). Announce the
    /// host's automation mode via [`HostAutomationState::set_automation_mode`],
    /// or use the [`set_automation_mode`](Self::set_automation_mode) convenience.
    pub fn automation_state(&self) -> Option<&dyn HostAutomationState> {
        self.automation_state.as_deref()
    }

    /// The render-mode capability (Direction C-in), or `None` when the backend
    /// carries no route to it.
    ///
    /// Every backend this crate builds fills the slot: the subprocess one for
    /// all three out-of-process formats, and `InProcessVst2Backend` for the
    /// in-crate VST2 path. `None` is reserved for an out-of-crate headless
    /// loader that passes it explicitly to
    /// [`from_backend`](Self::from_backend).
    pub fn render_mode(&self) -> Option<&dyn HostRenderMode> {
        self.render_mode.as_deref()
    }

    /// The preset capability, or `None` when this handle carries no route to
    /// presets at all.
    ///
    /// Distinct from "the plugin has no presets", which is an *empty list* from
    /// a present capability, and from "this format cannot enumerate", which is
    /// `Features::PRESET_LIST` on [`loaded`](Self::loaded). Three different
    /// answers, and a UI showing an empty browser wants to tell them apart.
    pub fn presets_capability(&self) -> Option<&dyn HostPresets> {
        self.presets.as_deref()
    }

    /// Every preset the plugin advertises.
    ///
    /// `None` when this handle has no preset route; `Some(vec![])` when it has
    /// one and the plugin listed nothing — which for CLAP is the normal state,
    /// since its discovery extension is not bound. Read
    /// `Features::PRESET_LIST` to tell "listed nothing" from "cannot be asked".
    pub fn presets(&self) -> Option<Vec<Preset>> {
        self.presets.as_deref().map(|p| p.presets())
    }

    /// Ask the plugin to load one, by an id [`presets`](Self::presets) produced.
    ///
    /// `false` when this handle carries no preset route *or* the plugin
    /// declined. Those collapse deliberately — both mean the preset did not
    /// load, and a caller must leave its selection where it was either way.
    /// One that needs to tell them apart reads `Features::PRESET_LOAD`.
    ///
    /// Never construct a [`PresetId`] to pass here. It is opaque, and three of
    /// the four formats number presets in a space that is not a position in the
    /// list — an invented id loads the wrong preset rather than failing.
    #[must_use = "a false return means the preset was not loaded"]
    pub fn load_preset(&self, id: &PresetId) -> bool {
        self.presets.as_deref().is_some_and(|p| p.load_preset(id))
    }

    /// Which preset the plugin considers current, when it will say.
    ///
    /// `None` covers all three of "no preset route", "the format has no query"
    /// (VST3, CLAP) and "the plugin declined" — never "the first one".
    pub fn current_preset(&self) -> Option<PresetId> {
        self.presets.as_deref().and_then(|p| p.current_preset())
    }

    /// Tell the plugin whether it is rendering under realtime pressure.
    ///
    /// Set this **before** a bounce pulls blocks: a plugin may spend more per
    /// block once it knows there is no deadline, and three of the four formats
    /// can only take the change while deactivated.
    ///
    /// `false` when this handle carries no render-mode route *or* the plugin
    /// declined. Those collapse deliberately — both mean the render is
    /// unchanged — and a caller that needs to tell them apart reads
    /// [`Features::RENDER_MODE`](crate::protocol::Features) on
    /// [`loaded`](Self::loaded).
    #[must_use = "a false return means the render mode was not applied"]
    pub fn set_render_mode(&self, mode: crate::protocol::RenderMode) -> bool {
        self.render_mode
            .as_deref()
            .is_some_and(|r| r.set_render_mode(mode))
    }

    // ---- Meta -------------------------------------------------------------

    /// `true` if this plugin exposes an embeddable editor (post-load truth).
    pub fn has_editor(&self) -> bool {
        self.editor.is_some()
    }

    /// Catalog identity (id, name, vendor, version, native class, editor).
    pub fn descriptor(&self) -> &PluginDescriptor {
        &self.descriptor
    }

    /// Engine-wiring data from load (per-bus channel widths, latency, f64).
    pub fn loaded(&self) -> &LoadedPlugin {
        &self.loaded
    }

    pub fn name(&self) -> &str {
        &self.descriptor.name
    }

    pub fn is_crashed(&self) -> bool {
        self.params.is_crashed()
    }

    // ---- Editor convenience (ergonomic wrappers over the editor slot) ------

    /// Embed the plugin's editor into `parent`. Pass anything that impls
    /// [`HasWindowHandle`] — Bevy windows, winit windows, wgpu surfaces.
    ///
    /// Returns the editor's requested size on success; otherwise a structured
    /// [`EditorError`] — including [`EditorError::GuiNotSupported`] when this
    /// plugin has no editor (`editor()` is `None`).
    pub fn open_editor(&self, parent: impl HasWindowHandle) -> Result<EditorSize, EditorError> {
        let Some(editor) = self.editor.as_deref() else {
            return Err(EditorError::GuiNotSupported {
                format: self.descriptor.class.format_name().to_string(),
            });
        };
        let raw = parent
            .window_handle()
            .map_err(|e| EditorError::PluginError(format!("failed to get window handle: {e}")))?
            .as_raw();
        let ptr = crate::util::window::extract_platform_ptr(raw)?;
        editor.open_editor(ptr)
    }

    /// Open the editor as a floating window the plugin owns.
    ///
    /// For a plugin whose [`Features::EDITOR_FLOATING`](crate::protocol::Features)
    /// bit is set — CLAP plugins that cannot embed. Takes no parent and returns
    /// no size: the window is the plugin's, so the host neither supplies nor
    /// lays it out. Close it with the same
    /// [`close_editor`](Self::close_editor) an embedded editor uses.
    pub fn open_floating_editor(&self) -> Result<(), EditorError> {
        let Some(editor) = self.editor.as_deref() else {
            return Err(EditorError::GuiNotSupported {
                format: self.descriptor.class.format_name().to_string(),
            });
        };
        editor.open_floating_editor()
    }

    pub fn close_editor(&self) -> &Self {
        if let Some(editor) = self.editor.as_deref() {
            editor.close_editor();
        }
        self
    }

    /// Call periodically (~30Hz) while editor is open.
    pub fn editor_idle(&self) -> &Self {
        if let Some(editor) = self.editor.as_deref() {
            editor.editor_idle();
        }
        self
    }

    /// Call after `open_editor`.
    pub fn editor_capabilities(&self) -> EditorCapabilities {
        self.editor
            .as_deref()
            .map(|e| e.editor_capabilities())
            .unwrap_or_default()
    }

    /// Returns the snapped/clamped size the plugin applied.
    pub fn set_editor_size(&self, requested: EditorSize) -> Result<EditorSize, EditorError> {
        match self.editor.as_deref() {
            Some(editor) => editor.set_editor_size(requested),
            None => Err(EditorError::GuiNotSupported {
                format: self.descriptor.class.format_name().to_string(),
            }),
        }
    }

    pub fn poll_editor_resize_request(&self) -> Option<EditorSize> {
        self.editor.as_deref()?.poll_editor_resize_request()
    }

    // ---- State convenience -------------------------------------------------

    pub fn save_state(&self) -> Option<Vec<u8>> {
        self.state.save_state()
    }

    pub fn load_state(&self, data: &[u8]) -> &Self {
        self.state.load_state(data);
        self
    }

    // ---- Param convenience -------------------------------------------------

    pub fn parameters(&self) -> Option<Vec<ParameterInfo>> {
        self.params.parameter_descriptors()
    }

    /// `param_id` comes from [`ParameterInfo::id`] on this plugin's own
    /// [`parameters`](Self::parameters) list — see [`ParamAddress`] for why a
    /// bare number cannot stand in for it.
    pub fn parameter(&self, param_id: ParamAddress) -> Option<f32> {
        self.params.parameter_value(param_id)
    }

    /// Main-thread, fire-and-forget.
    pub fn set_parameter(&self, param_id: ParamAddress, value: f32) -> &Self {
        self.params.set_parameter_value(param_id, value);
        self
    }

    // ---- Automation-state convenience --------------------------------------

    /// Announce the host's automation [`AutomationMode`] to the plugin so its
    /// editor can update UI feedback. Returns `Ok(())` if delivered, or an
    /// [`EditorError`] — including [`EditorError::GuiNotSupported`] when this
    /// backend has no automation-state capability (`automation_state()` is
    /// `None`).
    pub fn set_automation_mode(&self, mode: AutomationMode) -> Result<(), EditorError> {
        match self.automation_state.as_deref() {
            Some(a) => a.set_automation_mode(mode),
            None => Err(EditorError::GuiNotSupported {
                format: self.descriptor.class.format_name().to_string(),
            }),
        }
    }

    // ---- MIDI --------------------------------------------------------------

    /// Producer handle for this plugin's MIDI inbox. Cheap to clone —
    /// `MidiSender` is `Arc`-backed. Send `MidiEvent`s through the
    /// returned sender; the audio thread polls them on the next
    /// process call.
    pub fn midi_sender(&self) -> MidiSender {
        self.midi_sender.clone()
    }

    // ---- Notify sinks (plugin → host reactions) ----------------------------

    /// Register a callback invoked when the plugin writes back a
    /// parameter value internally (preset load, automation, host
    /// write-back). Fires on the bridge thread for the out-of-process
    /// backend; on the GUI thread (during `editor_idle`) for the in-process
    /// VST2 backend. Move heavy work to another thread before touching graph
    /// state. Replaces any previous callback.
    pub fn on_parameter_changed<F: Fn(u32, f32) + Send + Sync + 'static>(&self, f: F) -> &Self {
        self.param_sink.set(f);
        self
    }

    /// Register a callback for **cosmetic** refresh signals: the host's cached
    /// *view* of some plugin state is stale ([`PluginRefresh::ParamValues`] /
    /// [`PluginRefresh::ParamTitles`]) and should be re-read, but the audio graph
    /// is unaffected. See [`Self::on_parameter_changed`] for thread caveats. Only
    /// the out-of-process backend emits these; replaces any previous callback.
    pub fn on_refresh<F: Fn(PluginRefresh) + Send + Sync + 'static>(&self, f: F) -> &Self {
        self.refresh_sink.set(f);
        self
    }

    /// Register a callback for **structural** invalidation signals: the plugin
    /// changed its latency ([`PluginInvalidation::Latency`]) or bus layout
    /// ([`PluginInvalidation::Io`]), or reloaded in place
    /// ([`PluginInvalidation::Reloaded`]) — the host must rewire and re-run
    /// latency compensation (PDC). Absorbs what used to be a separate
    /// latency-changed callback. See [`Self::on_parameter_changed`] for thread
    /// caveats. Only the out-of-process backend emits these; replaces any
    /// previous callback.
    pub fn on_invalidate<F: Fn(PluginInvalidation) + Send + Sync + 'static>(&self, f: F) -> &Self {
        self.invalidate_sink.set(f);
        self
    }

    /// Clear the parameter-changed callback (if any).
    pub fn clear_parameter_callback(&self) -> &Self {
        self.param_sink.clear();
        self
    }

    /// Clear the refresh callback (if any).
    pub fn clear_refresh_callback(&self) -> &Self {
        self.refresh_sink.clear();
        self
    }

    /// Clear the invalidate callback (if any).
    pub fn clear_invalidate_callback(&self) -> &Self {
        self.invalidate_sink.clear();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A backend that reaches presets, standing in for a format layer that is
    /// not wired yet. Doubles as proof the capability is implementable from
    /// outside this crate's own backends.
    struct FakePresets {
        listed: Vec<Preset>,
        accepts: bool,
    }

    impl HostPresets for FakePresets {
        fn presets(&self) -> Vec<Preset> {
            self.listed.clone()
        }
        fn load_preset(&self, _id: &PresetId) -> bool {
            self.accepts
        }
        fn current_preset(&self) -> Option<PresetId> {
            None
        }
    }

    fn handle_with(presets: Option<Arc<dyn HostPresets>>) -> PluginHandle {
        // Only the preset slot is under test; the rest of the handle is built
        // from the same fake so no subprocess is needed.
        struct Inert;
        impl HostParams for Inert {
            fn parameter_descriptors(&self) -> Option<Vec<ParameterInfo>> {
                None
            }
            fn parameter_value(&self, _id: ParamAddress) -> Option<f32> {
                None
            }
            fn set_parameter_value(&self, _id: ParamAddress, _value: f32) {}
            fn is_crashed(&self) -> bool {
                false
            }
        }
        impl HostState for Inert {
            fn save_state(&self) -> Option<Vec<u8>> {
                None
            }
            fn load_state(&self, _data: &[u8]) {}
        }

        let (sender, _rx) =
            tutti_midi_runtime::MidiMailbox::pair(tutti_midi_types::MidiUnitId::next());
        PluginHandle::from_backend(
            Arc::new(Inert),
            OptionalCapabilities {
                presets,
                ..Default::default()
            },
            PluginDescriptor::default(),
            LoadedPlugin::default(),
            ParameterChangeSink::default(),
            sender,
        )
    }

    /// No preset route and an empty preset list are different answers.
    ///
    /// `None` means this handle cannot reach presets at all; `Some(vec![])`
    /// means it asked and the plugin listed nothing — the normal state for
    /// CLAP, whose discovery extension is not bound. Collapsing them to an
    /// empty vec would make a UI show "no presets" for a plugin it never
    /// asked, which is the same unprobed-versus-declined confusion
    /// `FeatureReport` exists to prevent.
    #[test]
    fn no_preset_route_is_not_an_empty_preset_list() {
        assert!(
            handle_with(None).presets().is_none(),
            "a handle with no preset capability must report None, not an empty list"
        );

        let empty = handle_with(Some(Arc::new(FakePresets {
            listed: Vec::new(),
            accepts: false,
        })));
        assert_eq!(
            empty.presets(),
            Some(Vec::new()),
            "a capability that listed nothing must report an empty list, not None"
        );
    }

    /// A refused load and an absent route both report `false`.
    ///
    /// Deliberate: both mean the preset did not load, and a caller must leave
    /// its selection where it was either way. The test pins that a *successful*
    /// load is the only `true`, so the collapse cannot hide one.
    #[test]
    fn only_an_accepted_load_reports_true() {
        let id = PresetId::Number(0);

        assert!(
            !handle_with(None).load_preset(&id),
            "no route is not a load"
        );

        let refusing = handle_with(Some(Arc::new(FakePresets {
            listed: Vec::new(),
            accepts: false,
        })));
        assert!(
            !refusing.load_preset(&id),
            "a refusal must not report success"
        );

        let accepting = handle_with(Some(Arc::new(FakePresets {
            listed: Vec::new(),
            accepts: true,
        })));
        assert!(
            accepting.load_preset(&id),
            "an accepted load must report true"
        );
    }

    /// The listed presets reach the caller unchanged.
    ///
    /// Pins that the handle is a pass-through: it must not sort, dedupe or
    /// renumber. A preset's id is opaque and its order is the plugin's, so any
    /// rearrangement here would desynchronize a UI's row index from the id it
    /// hands back.
    #[test]
    fn the_plugins_own_list_reaches_the_caller_verbatim() {
        let listed = vec![
            Preset::new(PresetId::Number(9000), "Sparse"),
            Preset::new(PresetId::Number(0), "First"),
        ];
        let handle = handle_with(Some(Arc::new(FakePresets {
            listed: listed.clone(),
            accepts: true,
        })));
        assert_eq!(
            handle.presets(),
            Some(listed),
            "the handle must not reorder or renumber what the plugin listed"
        );
    }
}
