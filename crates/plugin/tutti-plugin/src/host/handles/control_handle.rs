use crate::error::EditorError;
use crate::host::handles::capabilities::{
    HostAutomationState, HostEditor, HostParams, HostRenderMode, HostState,
};
use crate::host::ipc_client::audio::{PluginInvalidation, PluginRefresh};
use crate::host::node::{InvalidateSink, ParameterChangeSink, RefreshSink};
use crate::protocol::AutomationMode;
use crate::protocol::{LoadedPlugin, ParamAddress, ParameterInfo, PluginDescriptor};
use crate::util::window::{EditorCapabilities, EditorSize};
use raw_window_handle::HasWindowHandle;
use std::sync::Arc;
use tutti_midi_runtime::MidiSender;

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
            render_mode: Some(backend),
            descriptor: client.descriptor().clone(),
            loaded: client.loaded().clone(),
            param_sink: client.param_sink().clone(),
            refresh_sink: client.refresh_sink().clone(),
            invalidate_sink: client.invalidate_sink().clone(),
            midi_sender: client.midi_sender(),
        }
    }

    /// Construct from an in-process backend that implements the always-present
    /// capabilities, plus an optional editor. Used by every in-process loader —
    /// the in-crate VST2 path passes `Some(backend)` for the editor; a headless
    /// out-of-crate loader passes `None`.
    ///
    /// `backend: Arc<B>` is coerced into the `params`/`state` slots at the call
    /// site (both are clones of the same object), so shared state stays intact.
    pub fn from_backend<B: HostParams + HostState + 'static>(
        backend: Arc<B>,
        editor: Option<Arc<dyn HostEditor>>,
        descriptor: PluginDescriptor,
        loaded: LoadedPlugin,
        param_sink: ParameterChangeSink,
        midi_sender: MidiSender,
    ) -> Self {
        Self {
            params: backend.clone(),
            state: backend,
            editor,
            // In-process backends don't implement the VST3-style
            // automation-state advisory, so `automation_state()` is `None`.
            automation_state: None,
            // The in-process VST2 node owns the render mode itself — it answers
            // `audioMasterGetCurrentProcessLevel` from its own `HostState`, and
            // this handle has no route to that. `Plugin::set_render_mode`
            // reaches it directly.
            render_mode: None,
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
            render_mode: Some(backend),
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

    /// The render-mode capability (Direction C-in), or `None` when this handle
    /// has no route to it — the in-process VST2 node owns its own, reachable
    /// through `Plugin::set_render_mode`.
    pub fn render_mode(&self) -> Option<&dyn HostRenderMode> {
        self.render_mode.as_deref()
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
