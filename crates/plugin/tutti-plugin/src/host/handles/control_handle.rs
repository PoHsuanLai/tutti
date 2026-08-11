//! [`PluginHandle`] — the control-side face of a loaded plugin.
//!
//! Assembled from whichever capability traits its backend implements, so a
//! capability the backend lacks reads as `None` rather than erroring at the call.
//! Audio never travels through here; this is the off-RT half.

use crate::error::EditorError;
use crate::host::handles::capabilities::{
    HostAutomationState, HostEditor, HostParams, HostPresets, HostRenderMode, HostState,
};
use crate::host::ipc_client::audio::{PluginInvalidation, PluginRefresh};
use crate::host::node::{InvalidateSink, ParameterChangeSink, RefreshSink};
use crate::protocol::AutomationMode;
use crate::protocol::{
    ChannelTopology, LayoutSupport, LoadedPlugin, PluginDescriptor, PresetSupport,
};
use crate::util::window::EditorSize;
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
/// ```no_run
/// # use std::sync::Arc;
/// # use tutti_midi_runtime::MidiSender;
/// # use tutti_plugin::backend::{HostEditor, HostParams, HostState, ParameterChangeSink};
/// # use tutti_plugin::handles::{OptionalCapabilities, PluginHandle};
/// # use tutti_plugin::server::{LoadedPlugin, PluginDescriptor};
/// # fn ex<B: HostParams + HostState + 'static>(
/// #     backend: Arc<B>,
/// #     editor: Arc<dyn HostEditor>,
/// #     descriptor: PluginDescriptor,
/// #     loaded: LoadedPlugin,
/// #     param_sink: ParameterChangeSink,
/// #     midi_sender: MidiSender,
/// # ) -> PluginHandle {
/// // Hosts an editor; carries neither render mode nor presets.
/// PluginHandle::from_backend(
///     backend,
///     OptionalCapabilities { editor: Some(editor), ..Default::default() },
///     descriptor, loaded, param_sink, midi_sender,
/// )
/// # }
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

/// Whether a plugin is still answering — [`PluginHandle::status`].
///
/// Two variants because the engine can only speak to what it can *detect*. A
/// dead bridge is a fact latched at a known site with a known reason; a plugin
/// that is merely misbehaving is a judgement made by counting failed calls, and
/// how many failures over how long is the host's policy, not the engine's. See
/// [`PluginHandle::status`] for why `Alive` must not be read as "healthy".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginStatus {
    /// The bridge is up. **Not** a promise the plugin is behaving — only that
    /// it has not died in a way the engine can see.
    Alive,
    /// The plugin is gone and is not coming back: the subprocess never
    /// connected, failed the handshake, or its stream dropped mid-session.
    ///
    /// Terminal. The engine offers no relaunch, so recovery means loading a
    /// replacement and restoring whatever state was captured while the plugin
    /// was alive.
    Dead {
        /// Why the plugin died, latched at the detection site.
        cause: String,
    },
}

impl PluginStatus {
    /// `true` for [`Dead`](Self::Dead), for a call site that wants the bool.
    pub fn is_dead(&self) -> bool {
        matches!(self, Self::Dead { .. })
    }

    /// Why the plugin died, or `None` while it is alive.
    pub fn cause(&self) -> Option<&str> {
        match self {
            Self::Dead { cause } => Some(cause),
            Self::Alive => None,
        }
    }
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
///
/// # What each call costs
///
/// Nothing in a method's signature says whether it touches the plugin, so the
/// surface divides into three tiers that a caller has to know about. Getting
/// this wrong is not a slow frame — it is a **ten-second** one.
///
/// **Free.** [`descriptor`](Self::descriptor), [`loaded`](Self::loaded),
/// [`name`](Self::name), [`has_editor`](Self::has_editor),
/// [`preset_support`](Self::preset_support),
/// [`layout_support`](Self::layout_support), the bus-topology accessors, and
/// every capability accessor read a struct filled in at load time.
/// [`status`](Self::status) and [`is_crashed`](Self::is_crashed) read an atomic.
/// Call these per frame.
///
/// **Fire-and-forget.** [`HostParams::set_parameter_value`],
/// [`set_render_mode`](Self::set_render_mode) and
/// [`set_automation_mode`](Self::set_automation_mode) push onto a lock-free
/// queue and return without waiting. A knob turn must never stall a UI, so
/// these deliberately have no reply to wait for — and therefore no way to
/// report that the plugin refused.
///
/// **Blocking round-trips.** [`HostState::save_state`],
/// [`HostState::load_state`], [`HostParams::parameter_descriptors`],
/// [`HostParams::parameter_value`], [`HostPresets::presets`],
/// [`HostPresets::load_preset`] and [`HostPresets::current_preset`] each send a
/// command to the subprocess and **wait for the reply** — five seconds for the
/// parameter and preset calls, ten for state. A wedged plugin spends the whole
/// timeout before returning `None`.
///
/// The last group is why this type is `Send + Sync + Clone`: clone the handle
/// into whatever the host already uses for off-thread work and call it there.
/// Under Bevy that is one line —
/// `AsyncComputeTaskPool::get().spawn(async move { handle.state().save_state() })`
/// — held in a component and polled next frame, the shape `bevy_tutti`'s plugin
/// load and state snapshot both use.
///
/// These are deliberately **not** `async` and do not return futures. That would
/// put an executor choice inside a library that needs none, and would not save
/// a host any work: the reply arrives on a `crossbeam` channel, so a future
/// would have to be driven by *something* — which is the same something that
/// can run the blocking call directly.
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
    ///
    /// Ask [`preset_support`](Self::preset_support) what the surface can *do*
    /// before rendering one; this is the route to doing it.
    pub fn presets(&self) -> Option<&dyn HostPresets> {
        self.presets.as_deref()
    }

    /// What this plugin's preset surface can do — one call instead of two
    /// capability bits and two method returns.
    ///
    /// Match on it to decide what to render:
    ///
    /// ```no_run
    /// # use tutti_plugin::{handles::PluginHandle, PresetSupport};
    /// # fn ex(handle: &PluginHandle) {
    /// match handle.preset_support() {
    ///     // Browser; clicking loads.
    ///     PresetSupport::Full => {}
    ///     // File picker, not an empty browser — CLAP enumerates nothing.
    ///     PresetSupport::LoadByPath => {}
    ///     // Read-only list.
    ///     PresetSupport::ListOnly => {}
    ///     // Hide it.
    ///     PresetSupport::None => {}
    /// }
    /// # }
    /// ```
    ///
    /// Derived from the capability report rather than from the preset list,
    /// because an empty list is ambiguous on its own: a plugin that declined
    /// and one this host never asked both list nothing, and only the first
    /// should hide the browser.
    ///
    /// Stays on the handle rather than moving to [`HostPresets`] because it
    /// reads [`loaded`](Self::loaded) — the capability object cannot see the
    /// feature bits, and "no route at all" is a fact about the handle rather
    /// than an answer any capability could give.
    pub fn preset_support(&self) -> PresetSupport {
        if self.presets.is_none() {
            // No route at all: the plugin was never asked anything, whatever
            // its own features say.
            return PresetSupport::None;
        }
        let report = crate::protocol::FeatureReport::new(self.loaded.probed, self.loaded.features);
        PresetSupport::from_report(&report)
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
    /// [`loaded`](Self::loaded). Kept rather than left to
    /// [`render_mode`](Self::render_mode) precisely because that collapse is
    /// the useful answer: every caller so far wants the one bool.
    #[must_use = "a false return means the render mode was not applied"]
    pub fn set_render_mode(&self, mode: crate::protocol::RenderMode) -> bool {
        self.render_mode
            .as_deref()
            .is_some_and(|r| r.set_render_mode(mode))
    }

    // ---- Meta -------------------------------------------------------------

    /// Whether the plugin is still answering, and why not if it is not.
    ///
    /// The honest form of [`is_crashed`](Self::is_crashed), which is a `bool`
    /// that cannot carry a reason: the `BridgeError` behind a death is dropped
    /// as soon as the failing call returns, so a host that polled the flag
    /// could only ever report a placeholder. The cause is latched where the
    /// crash is noticed, so this answers even for a plugin that died before the
    /// host installed a listener — the connect- and handshake-failure cases,
    /// which are the common ones for a bad install.
    ///
    /// **Two variants, not three.** There is deliberately no `Failing` here.
    /// A peer that answers a control call with a well-formed reply of the wrong
    /// kind leaves this reporting [`Alive`](PluginStatus::Alive) while the call
    /// returns `None`, and that mode has no detection site to latch from — it
    /// is only visible by counting failed calls, which is a policy (how many,
    /// how fast) belonging to the host rather than to the engine. Pair this
    /// with your own debounce if you need one; do not read `Alive` as "healthy".
    ///
    /// For prompt notice rather than polling, subscribe with
    /// [`on_invalidate`](Self::on_invalidate) and match
    /// [`PluginInvalidation::Crashed`].
    pub fn status(&self) -> PluginStatus {
        match self.params.crash_cause() {
            Some(cause) => PluginStatus::Dead { cause },
            // A backend that reports the flag without a cause still reports
            // death — `crash_cause` is defaulted for backends that cannot say
            // why, and losing the death because the reason is missing would be
            // the worse failure.
            None if self.params.is_crashed() => PluginStatus::Dead {
                cause: "the backend reported a crash without a cause".to_string(),
            },
            None => PluginStatus::Alive,
        }
    }

    /// `true` if this plugin exposes an embeddable editor (post-load truth).
    ///
    /// A bool rather than `editor().is_some()` at the call site because the
    /// question is asked while deciding whether to *offer* a window — often per
    /// frame — and it answers without materialising the trait object.
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

    /// What is known about this plugin's channel placement.
    ///
    /// The one call a caller makes before deciding whether to speak in speaker
    /// names or channel numbers:
    ///
    /// ```no_run
    /// # use tutti_plugin::handles::PluginHandle;
    /// # use tutti_plugin_types::LayoutSupport;
    /// # fn ex(handle: &PluginHandle) {
    /// match handle.layout_support() {
    ///     // Name every channel's speaker.
    ///     LayoutSupport::Full => {}
    ///     // Per bus: name what answered, number the rest. Do not assume the
    ///     // gaps are stereo.
    ///     LayoutSupport::Partial => {}
    ///     // Channel numbers only.
    ///     LayoutSupport::None => {}
    /// }
    /// # }
    /// ```
    ///
    /// **Reporting only.** No variant means a layout can be *changed*: nothing
    /// above the format hosts proposes one today, and naming a capability that
    /// cannot be reached is the write-only shape this work exists to remove.
    /// See [`LayoutSupport`].
    pub fn layout_support(&self) -> LayoutSupport {
        LayoutSupport::of(&self.loaded)
    }

    /// Which speaker each channel of one input bus feeds.
    ///
    /// `None` when that bus reported no placement — the plugin declined, the
    /// format cannot say, or it names a speaker this vocabulary lacks. Never
    /// "no speakers": a bus with no channels is `Some` of an empty topology.
    ///
    /// Reads the same per-bus data [`layout_support`](Self::layout_support)
    /// summarises, so a caller that got [`LayoutSupport::Partial`] uses this to
    /// find which buses actually answered.
    pub fn input_bus_topology(&self, bus: usize) -> Option<&ChannelTopology> {
        self.loaded.input_bus_topology(bus)
    }

    /// Which speaker each channel of one output bus feeds. See
    /// [`input_bus_topology`](Self::input_bus_topology).
    pub fn output_bus_topology(&self, bus: usize) -> Option<&ChannelTopology> {
        self.loaded.output_bus_topology(bus)
    }

    /// The plugin's display name, as its descriptor reports it.
    pub fn name(&self) -> &str {
        &self.descriptor.name
    }

    /// Whether the plugin has died. See [`PluginStatus::Dead`] for what is
    /// recoverable.
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

    /// Close the editor, if one is open.
    ///
    /// A no-op without an editor route, so a caller tearing a window down need
    /// not first ask whether there was one. Returns `&Self` for chaining with
    /// the other editor calls.
    pub fn close_editor(&self) -> &Self {
        if let Some(editor) = self.editor.as_deref() {
            editor.close_editor();
        }
        self
    }

    /// Call periodically (~30Hz) while editor is open.
    ///
    /// Like [`close_editor`](Self::close_editor), a no-op without an editor:
    /// this is driven from a per-frame system that should not branch on a
    /// capability that cannot change after load.
    pub fn editor_idle(&self) -> &Self {
        if let Some(editor) = self.editor.as_deref() {
            editor.editor_idle();
        }
        self
    }

    /// Returns the snapped/clamped size the plugin applied.
    ///
    /// Converts "no editor" into [`EditorError::GuiNotSupported`] rather than
    /// making the caller unwrap an `Option` first, matching
    /// [`open_editor`](Self::open_editor).
    pub fn set_editor_size(&self, requested: EditorSize) -> Result<EditorSize, EditorError> {
        match self.editor.as_deref() {
            Some(editor) => editor.set_editor_size(requested),
            None => Err(EditorError::GuiNotSupported {
                format: self.descriptor.class.format_name().to_string(),
            }),
        }
    }

    /// A size the *plugin* has asked to become, if it asked since the last
    /// poll. Call it beside [`editor_idle`](Self::editor_idle) on the host's
    /// frame loop.
    ///
    /// The other half of the conversation [`set_editor_size`](Self::set_editor_size)
    /// starts: that one is the host resizing the plugin, this one is the plugin
    /// asking the host to resize the window it lives in. A plugin with a
    /// zoom control or a collapsible panel drives its own size this way, and a
    /// host that never polls leaves the editor clipped inside a window that
    /// does not match it.
    ///
    /// [`None`] rather than a `Result`, unlike its sibling: "no editor" and "the
    /// plugin has not asked" are the same answer to a poll — nothing to do —
    /// whereas `set_editor_size` is a request that deserves to be told it went
    /// nowhere.
    ///
    /// Exists for symmetry rather than to remove duplication — there is one
    /// caller today — because [`set_editor_size`](Self::set_editor_size) carries
    /// the same convenience, and the asymmetry reads as "one of these is not
    /// part of the
    /// handle API".
    pub fn poll_editor_resize_request(&self) -> Option<EditorSize> {
        self.editor.as_deref()?.poll_editor_resize_request()
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
    /// latency compensation (PDC). Latency and IO changes share one callback
    /// because they demand the identical host response. See
    /// [`Self::on_parameter_changed`] for thread caveats. Only the
    /// out-of-process backend emits these; replaces any previous callback.
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
    use crate::protocol::{Normalized, ParamAddress, ParameterInfo, Preset, PresetId};

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
        handle_with_loaded(presets, LoadedPlugin::default())
    }

    fn handle_with_loaded(
        presets: Option<Arc<dyn HostPresets>>,
        loaded: LoadedPlugin,
    ) -> PluginHandle {
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
            fn set_parameter_value(&self, _id: ParamAddress, _value: Normalized) {}
            fn is_crashed(&self) -> bool {
                false
            }
        }
        impl HostState for Inert {
            fn save_state(&self) -> Option<Vec<u8>> {
                None
            }
            /// `NoStateRoute`, not a silent success: this backend carries no
            /// plugin at all, so a caller must not read "state loaded" from it.
            fn load_state(
                &self,
                _data: &[u8],
            ) -> std::result::Result<(), crate::error::StateError> {
                Err(crate::error::StateError::NoStateRoute)
            }
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
            loaded,
            ParameterChangeSink::default(),
            sender,
        )
    }

    /// A handle with no preset route reports `None`, whatever the plugin's own
    /// features claim.
    ///
    /// The route and the capability are independent facts, and the route wins:
    /// a `LoadedPlugin` carrying both preset bits set means nothing if nothing
    /// carries the call. Deriving `preset_support` from the bits alone would
    /// have a UI offer a browser that cannot be driven.
    #[test]
    fn no_preset_route_reports_no_support() {
        // Both bits probed and set — the plugin claims full preset support —
        // but nothing carries the call. Without a `LoadedPlugin` saying yes,
        // this test could not tell the route guard from the empty default.
        let mut loaded = LoadedPlugin::default();
        loaded.probed =
            crate::protocol::Features::PRESET_LIST | crate::protocol::Features::PRESET_LOAD;
        loaded.features = loaded.probed;

        let handle = handle_with_loaded(None, loaded.clone());
        assert_eq!(
            handle.preset_support(),
            PresetSupport::None,
            "the route is missing, so the plugin's own claim cannot be honoured"
        );

        // The same plugin *with* a route reports what it claims — otherwise the
        // assertion above would hold for the wrong reason.
        let wired = handle_with_loaded(
            Some(Arc::new(FakePresets {
                listed: Vec::new(),
                accepts: true,
            })),
            loaded,
        );
        assert_eq!(wired.preset_support(), PresetSupport::Full);
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
            empty.presets().map(|p| p.presets()),
            Some(Vec::new()),
            "a capability that listed nothing must report an empty list, not None"
        );
    }

    /// Only an accepted load reports `true`, and an absent route is not a
    /// refusal.
    ///
    /// Reaching presets through the capability keeps three answers apart that a
    /// bare `-> bool` collapses into two: `None` (no preset route at all),
    /// `Some(false)` (a route that refused) and `Some(true)`. A caller leaves
    /// its selection alone for both of the first two, but only the second is
    /// the plugin saying no — and a UI reporting "this plugin cannot load
    /// presets" must tell them apart.
    #[test]
    fn only_an_accepted_load_reports_true() {
        let id = PresetId::Number(0);

        assert!(
            handle_with(None).presets().is_none(),
            "no route must stay distinct from a refusal, not collapse into one"
        );

        let refusing = handle_with(Some(Arc::new(FakePresets {
            listed: Vec::new(),
            accepts: false,
        })));
        assert_eq!(
            refusing.presets().map(|p| p.load_preset(&id)),
            Some(false),
            "a refusal must report a present route that said no"
        );

        let accepting = handle_with(Some(Arc::new(FakePresets {
            listed: Vec::new(),
            accepts: true,
        })));
        assert_eq!(
            accepting.presets().map(|p| p.load_preset(&id)),
            Some(true),
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
            handle.presets().map(|p| p.presets()),
            Some(listed),
            "the handle must not reorder or renumber what the plugin listed"
        );
    }
}
