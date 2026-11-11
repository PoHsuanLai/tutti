//! [`PluginHandle`] — the control-side face of a loaded plugin.
//!
//! Assembled from whichever capability traits its backend implements, so a
//! capability the backend lacks reads as `None` rather than erroring at the call.
//! Audio never travels through here; this is the off-RT half.

use crate::{InvalidateSink, ParameterChangeSink, RefreshSink};
use raw_window_handle::HasWindowHandle;
use std::sync::Arc;
use tutti_plugin_types::BlockCounts;
use tutti_plugin_types::MainThread;
use tutti_plugin_types::{
    AnyThread, AutomationMode, Control, EditorCapabilities, EditorError, Health, PluginError,
    StateError, WindowHandle,
};
use tutti_plugin_types::{ContentScale, EditorSize};
use tutti_plugin_types::{
    LayoutSupport, LoadedPlugin, Normalized, ParamId, PluginBus, PluginDescriptor, Preset,
    PresetId, PresetSupport,
};
use tutti_plugin_types::{PluginInvalidation, PluginRefresh};

/// How a [`PluginHandle`] reaches a plugin's main-thread [`Control`].
///
/// A `Control` call takes the token and `&mut self`, and the plugin sits
/// behind a lock (in-process) or in the bridge (served), so it is lent to a
/// closure rather than handed out. A served plugin's control is `Clone` and
/// holds no lock, which is the blanket impl; an in-process plugin's is
/// `ControlBackend`, which locks the instance and pumps on
/// [`editor_idle`](Self::editor_idle).
pub trait MainThreadControl: Send + Sync {
    /// Runs `f` on the plugin's `Control`, or does not run it when the plugin
    /// is lost.
    fn lend(&self, f: &mut dyn FnMut(&mut dyn Control));

    /// The editor's tick, which an in-process plugin pairs with the pump of
    /// what the plugin changed about itself.
    fn editor_idle(&self, main: &MainThread);
}

impl<C: Control + Clone + Send + Sync> MainThreadControl for C {
    fn lend(&self, f: &mut dyn FnMut(&mut dyn Control)) {
        f(&mut self.clone());
    }

    fn editor_idle(&self, main: &MainThread) {
        self.lend(&mut |control| control.editor_idle(main));
    }
}

/// `f` on the lent control, and its answer; `None` when the plugin is lost.
fn lent<R>(control: &dyn MainThreadControl, f: impl FnOnce(&mut dyn Control) -> R) -> Option<R> {
    let mut f = Some(f);
    let mut answer = None;
    control.lend(&mut |c| answer = f.take().map(|f| f(c)));
    answer
}

/// A plugin's error as the editor's.
fn editor_error(error: PluginError) -> EditorError {
    match error {
        PluginError::Editor(editor) => editor,
        other => EditorError::PluginError(other.to_string()),
    }
}

/// Whether a plugin is still answering — [`PluginHandle::status`].
///
/// Two variants because the engine can only speak to what it can *detect*. A
/// dead bridge is a fact latched at a known site with a known reason; a plugin
/// that is merely misbehaving is a judgement made by counting failed calls, and
/// how many failures over how long is the host's policy, not the engine's. See
/// [`PluginHandle::status`] for why `Alive` must not be read as "healthy".
#[derive(Debug, Clone, PartialEq)]
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
    /// The plugin refused the sample rate the engine runs at. It stays
    /// loaded and renders silence, and it is retried at every later rate
    /// change (the error clears when it takes one).
    ///
    /// Not [`Dead`](Self::Dead): the declared latency stands, so the graph's
    /// plan does not change, and nothing needs reloading.
    RateRefused {
        /// The rate the plugin refused.
        rate: tutti_types::SampleRate,
        /// The plugin's reason, formatted subprocess-side.
        reason: String,
    },
}

impl PluginStatus {
    /// `true` for [`Dead`](Self::Dead), for a call site that wants the bool.
    #[must_use]
    pub fn is_dead(&self) -> bool {
        matches!(self, Self::Dead { .. })
    }

    /// Why the plugin died, or `None` while it is alive.
    pub fn cause(&self) -> Option<&str> {
        match self {
            Self::Dead { cause } => Some(cause),
            Self::Alive | Self::RateRefused { .. } => None,
        }
    }
}

/// Main-thread control handle for a loaded plugin.
///
/// Hosting-agnostic: it holds the plugin's [`AnyThread`] and [`Health`], and
/// its [`MainThreadControl`] when it has one, so out-of-process VST3/CLAP/AU
/// and in-process VST2 hosting share this surface.
///
/// Clone is cheap (`Arc`-based). Action methods return `&Self` for chaining.
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
/// **Fire-and-forget.** [`AnyThread::set_parameter_value`],
/// [`set_render_mode`](Self::set_render_mode) and
/// [`set_automation_mode`](Self::set_automation_mode) push onto a lock-free
/// queue and return without waiting. A knob turn must never stall a UI, so
/// these deliberately have no reply to wait for — and therefore no way to
/// report that the plugin refused.
///
/// **Blocking round-trips.** [`AnyThread::save_state`],
/// [`load_state`](Self::load_state), [`AnyThread::parameter_descriptors`],
/// [`AnyThread::parameter_value`], [`presets`](Self::presets),
/// [`load_preset`](Self::load_preset) and [`current_preset`](Self::current_preset) each send a
/// command to the subprocess and **wait for the reply** — five seconds for the
/// parameter and preset calls, ten for state. A wedged plugin spends the whole
/// timeout before returning `None`.
///
/// The last group is why this type is `Send + Sync + Clone`: clone the handle
/// into whatever the host already uses for off-thread work and call it there.
/// Under Bevy that is one line —
/// `AsyncComputeTaskPool::get().spawn(async move { handle.any_thread().save_state() })`
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
    any_thread: Arc<dyn AnyThread>,
    health: Arc<dyn Health>,
    control: Option<Arc<dyn MainThreadControl>>,
    descriptor: PluginDescriptor,
    loaded: LoadedPlugin,
    param_sink: ParameterChangeSink,
    refresh_sink: RefreshSink,
    invalidate_sink: InvalidateSink,
    server_pid: Option<u32>,
}

impl std::fmt::Debug for PluginHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginHandle")
            .field("descriptor", &self.descriptor)
            .field("loaded", &self.loaded)
            .field("param_sink", &self.param_sink)
            .field("refresh_sink", &self.refresh_sink)
            .field("invalidate_sink", &self.invalidate_sink)
            .field("server_pid", &self.server_pid)
            .finish_non_exhaustive()
    }
}

impl PluginHandle {
    /// Creates a handle over a backend: an in-process one, or the client of a
    /// served plugin, which adds its refresh sink and server pid below.
    ///
    /// `backend` is the plugin's [`AnyThread`] and [`Health`] (one object
    /// serves both). `control` is how the main-thread calls reach the plugin;
    /// `None` is a handle with no such route (no editor, no presets, no state
    /// restore), as a test double has.
    pub fn from_backend<B: AnyThread + Health + 'static>(
        backend: Arc<B>,
        control: Option<Arc<dyn MainThreadControl>>,
        descriptor: PluginDescriptor,
        loaded: LoadedPlugin,
        param_sink: ParameterChangeSink,
    ) -> Self {
        Self {
            any_thread: backend.clone(),
            health: backend,
            control,
            descriptor,
            loaded,
            param_sink,
            // The default sinks: an in-process backend emits no refresh signal,
            // and raises invalidations through `with_invalidate_sink`.
            refresh_sink: RefreshSink::default(),
            invalidate_sink: InvalidateSink::default(),
            // No subprocess to name unless `with_server_pid` says so.
            server_pid: None,
        }
    }

    /// This handle with `sink` as the one `on_invalidate` registers into: how
    /// an in-process backend that re-reads its plugin raises
    /// [`PluginInvalidation`]s.
    ///
    /// Public for `tutti-plugin`'s served-plugin handle, which also hears invalidations over its wire.
    pub fn with_invalidate_sink(mut self, sink: InvalidateSink) -> Self {
        self.invalidate_sink = sink;
        self
    }

    /// This handle with `sink` as the one `on_refresh` registers into: how a
    /// backend whose plugin raises cosmetic refreshes (the out-of-process
    /// client, over its wire) delivers [`PluginRefresh`]es.
    ///
    /// Public for `tutti-plugin`'s served-plugin handle, which hears refreshes over its wire.
    pub fn with_refresh_sink(mut self, sink: RefreshSink) -> Self {
        self.refresh_sink = sink;
        self
    }

    /// This handle with `pid` as the plugin server's process id
    /// ([`server_pid`](Self::server_pid)): for a backend that hosts the plugin
    /// in a process of its own.
    ///
    /// Public for `tutti-plugin`'s served-plugin handle, the only one with a subprocess to name.
    pub fn with_server_pid(mut self, pid: Option<u32>) -> Self {
        self.server_pid = pid;
        self
    }

    // ---- Accessors ---------------------------------------------------------

    /// What the plugin answers from any thread: the parameter catalog, reads
    /// and writes, `save_state`. See [`AnyThread`].
    pub fn any_thread(&self) -> &dyn AnyThread {
        self.any_thread.as_ref()
    }

    /// Returns what this plugin's preset surface can do, combining the preset
    /// route and the two preset capability bits.
    ///
    /// Match on it to decide what to render:
    ///
    /// ```no_run
    /// # use tutti_plugin_host::PluginHandle;
    /// # use tutti_plugin_types::PresetSupport;
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
    pub fn preset_support(&self) -> PresetSupport {
        if self.control.is_none() {
            // No route at all: the plugin was never asked anything, whatever
            // its own features say.
            return PresetSupport::None;
        }
        let report =
            tutti_plugin_types::FeatureReport::new(self.loaded.probed, self.loaded.features);
        PresetSupport::from_report(&report)
    }

    /// Tells the plugin whether it is rendering under realtime pressure.
    ///
    /// Set this **before** a bounce pulls blocks: a plugin may spend more per
    /// block once it knows there is no deadline, and three of the four formats
    /// can only take the change while deactivated.
    ///
    /// `false` when the plugin declined (or, in-process off the main thread,
    /// the main thread did not answer in time). Those collapse deliberately —
    /// both mean the render is unchanged — and a caller that needs to tell them
    /// apart reads [`Features::RENDER_MODE`](tutti_plugin_types::Features) on
    /// [`loaded`](Self::loaded).
    #[must_use = "a false return means the render mode was not applied"]
    pub fn set_render_mode(&self, mode: tutti_plugin_types::RenderMode) -> bool {
        self.any_thread.set_render_mode(mode)
    }

    // ---- Meta -------------------------------------------------------------

    /// Returns whether the plugin is still answering, and why not if it is not.
    ///
    /// Unlike [`is_crashed`](Self::is_crashed), this carries the reason. The
    /// cause is latched where the crash is noticed, so this answers even for a
    /// plugin that died before the host installed a listener — the connect-
    /// and handshake-failure cases, which are the common ones for a bad
    /// install.
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
        match self.health.crash_cause() {
            Some(cause) => PluginStatus::Dead { cause },
            // A backend that reports the flag without a cause still reports
            // death — `crash_cause` is defaulted for backends that cannot say
            // why, and losing the death because the reason is missing would be
            // the worse failure.
            None if self.health.is_crashed() => PluginStatus::Dead {
                cause: "the backend reported a crash without a cause".to_string(),
            },
            None => match self.health.rate_refusal() {
                Some((rate, reason)) => PluginStatus::RateRefused { rate, reason },
                None => PluginStatus::Alive,
            },
        }
    }

    /// Returns `true` if this plugin has an editor this handle can open.
    ///
    /// Cheap enough to call per frame when deciding whether to offer a
    /// window.
    #[must_use]
    pub fn has_editor(&self) -> bool {
        self.control.is_some()
    }

    /// Returns the plugin's catalog identity (id, name, vendor, version, class,
    /// editor).
    pub fn descriptor(&self) -> &PluginDescriptor {
        &self.descriptor
    }

    /// Returns what the plugin reported at load: per-bus channel widths,
    /// latency, `f64` support and capability bits.
    pub fn loaded(&self) -> &LoadedPlugin {
        &self.loaded
    }

    /// Returns the OS process id of the `plugin-server` hosting this plugin, or
    /// `None` for an in-process plugin.
    ///
    /// Fixed at load, so free to call. The subprocess lives until the last
    /// handle *and* the graph's node have both dropped, and it is then
    /// killed and reaped synchronously. So once they have all dropped, this id
    /// names no child of this process. A caller that probes it after that
    /// point is asking whether the subprocess was reaped, not what it is doing.
    pub fn server_pid(&self) -> Option<u32> {
        self.server_pid
    }

    /// Returns what is known about this plugin's channel placement.
    ///
    /// The one call a caller makes before deciding whether to speak in speaker
    /// names or channel numbers:
    ///
    /// ```no_run
    /// # use tutti_plugin_host::PluginHandle;
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
    /// **Reporting only.** No variant means a layout can be *changed*: the host
    /// has no call that proposes a layout to a plugin. See [`LayoutSupport`].
    pub fn layout_support(&self) -> LayoutSupport {
        LayoutSupport::of(&self.loaded)
    }

    /// Returns the plugin's input buses, in bus order: each one's width, its
    /// [`placement`](PluginBus::placement) when it reported one, and its
    /// [`role`](PluginBus::role).
    ///
    /// The per-bus data [`layout_support`](Self::layout_support) summarises,
    /// so a caller that got [`LayoutSupport::Partial`] reads here which buses
    /// answered.
    pub fn input_buses(&self) -> &[PluginBus] {
        &self.loaded.inputs
    }

    /// Returns the plugin's output buses. See
    /// [`input_buses`](Self::input_buses).
    pub fn output_buses(&self) -> &[PluginBus] {
        &self.loaded.outputs
    }

    /// Returns the plugin's display name, as its descriptor reports it.
    pub fn name(&self) -> &str {
        &self.descriptor.name
    }

    /// Returns `true` if the plugin has died. See [`status`](Self::status) for
    /// the reason and [`PluginStatus::Dead`] for what is recoverable.
    #[must_use]
    pub fn is_crashed(&self) -> bool {
        self.health.is_crashed()
    }

    /// Blocks the audio path abandoned and replies it took. See
    /// [`Health::block_counts`]. Atomic reads.
    #[must_use]
    pub fn block_counts(&self) -> BlockCounts {
        self.health.block_counts()
    }

    /// Ends the plugin's server process. The bridge then latches `cause`
    /// (unless it died of something else first), so [`status`](Self::status)
    /// turns [`Dead`](PluginStatus::Dead) as for any crash. Nothing for an
    /// in-process plugin. Not for the audio thread.
    pub fn terminate(&self, cause: &str) {
        self.health.terminate(cause);
    }

    /// How many parameter-change events the plugin sent that the host lost,
    /// over the plugin's life. Monotonic; the plugin's
    /// [`Control::dropped_reported_edits`]. A dropped event costs a
    /// stale readout, not audio, and the subprocess backend follows each drop
    /// with an [`on_refresh`](Self::on_refresh) of
    /// [`PluginRefresh::ParamValues`] so a host that re-reads loses nothing.
    pub fn dropped_parameter_events(&self) -> u64 {
        self.control
            .as_deref()
            .and_then(|control| lent(control, |c| c.dropped_reported_edits()))
            .unwrap_or(0)
    }

    // ---- State and presets (main thread) ----------------------------------

    /// Restores a blob produced by [`AnyThread::save_state`].
    ///
    /// # Errors
    ///
    /// A [`StateError`] if the state was not applied. A plugin declining a
    /// chunk ([`StateError::Rejected`]) is routine: it happens when a state
    /// saved by an older build is loaded into a newer one, when a file is
    /// truncated, or when a chunk from a different plugin is fed in. Report it
    /// to the user rather than leaving the plugin silently at defaults.
    /// [`StateError::NoStateRoute`] when this handle carries no main-thread
    /// route.
    pub fn load_state(&self, main: &MainThread, data: &[u8]) -> Result<(), StateError> {
        let Some(control) = self.control.as_deref() else {
            return Err(StateError::NoStateRoute);
        };
        match lent(control, |c| c.load_state(main, data)) {
            Some(Ok(())) => Ok(()),
            Some(Err(PluginError::StateFailed(error))) => Err(error),
            Some(Err(other)) => Err(StateError::Rejected(other.to_string())),
            None => Err(StateError::Rejected("the plugin is lost".into())),
        }
    }

    /// Every preset the plugin advertises, in its own order, or `None` when
    /// this handle carries no route to presets at all.
    ///
    /// Empty (`Some`) when the format cannot enumerate (CLAP), which is **not**
    /// the same as a plugin with no presets; read
    /// [`Features::PRESET_LIST`](tutti_plugin_types::Features::PRESET_LIST) on
    /// [`loaded`](Self::loaded) to tell them apart.
    pub fn presets(&self, main: &MainThread) -> Option<Vec<Preset>> {
        let control = self.control.as_deref()?;
        Some(lent(control, |c| c.presets(main)).unwrap_or_default())
    }

    /// Asks the plugin to load a preset, by an id [`presets`](Self::presets)
    /// produced (or, for CLAP, a path id). `None` when this handle carries no
    /// route to presets; `Some(false)` when the plugin refused (or is lost):
    /// leave the UI selection where it was.
    pub fn load_preset(&self, main: &MainThread, id: &PresetId) -> Option<bool> {
        let control = self.control.as_deref()?;
        Some(lent(control, |c| c.load_preset(main, id)).unwrap_or(false))
    }

    /// The preset the plugin considers current, when it will say. `None` means
    /// the format has no query for it, the plugin declined, or this handle has
    /// no route — never "the first one". See [`PresetId`].
    pub fn current_preset(&self, main: &MainThread) -> Option<PresetId> {
        lent(self.control.as_deref()?, |c| c.current_preset(main))?
    }

    // ---- Editor convenience (ergonomic wrappers over the editor slot) ------

    /// Embeds the plugin's editor into `parent` and returns the size the editor
    /// asked for.
    ///
    /// Pass anything that implements [`HasWindowHandle`]: Bevy windows, winit
    /// windows, wgpu surfaces. `scale` is that window's backing-store scale
    /// (`winit::Window::scale_factor`, Bevy's `Window::scale_factor`): a
    /// window handle carries none, and a plugin on Windows or X11 cannot
    /// read it itself. Call on the main thread.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::GuiNotSupported`] when this handle has no editor
    /// route, an [`EditorError`] if `parent`'s window
    /// handle is unavailable or of an unsupported platform, and otherwise any
    /// error of [`EditorWindow::open_editor`](tutti_plugin_types::EditorWindow::open_editor).
    pub fn open_editor(
        &self,
        main: &MainThread,
        parent: impl HasWindowHandle,
        scale: ContentScale,
    ) -> Result<EditorSize, EditorError> {
        let control = self.editor_route()?;
        let parent = WindowHandle::from_window(&parent)?;
        Self::editor_call(control, |c| c.open_editor(main, parent, scale))
    }

    /// Opens the editor as a floating window the plugin owns.
    ///
    /// For a plugin whose [`Features::EDITOR_FLOATING`](tutti_plugin_types::Features)
    /// bit is set — CLAP plugins that cannot embed. Takes no parent and returns
    /// no size: the window is the plugin's, so the host neither supplies nor
    /// lays it out. Close it with the same
    /// [`close_editor`](Self::close_editor) an embedded editor uses.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::GuiNotSupported`] when this handle has no editor
    /// route, and otherwise any error of
    /// [`EditorWindow::open_floating_editor`](tutti_plugin_types::EditorWindow::open_floating_editor),
    /// such as a format without floating editors.
    pub fn open_floating_editor(&self, main: &MainThread) -> Result<(), EditorError> {
        let control = self.editor_route()?;
        Self::editor_call(control, |c| c.open_floating_editor(main))
    }

    /// Closes the editor, if one is open.
    ///
    /// A no-op without an editor route, so a caller tearing a window down need
    /// not first ask whether there was one. Returns `&Self` for chaining with
    /// the other editor calls.
    pub fn close_editor(&self, main: &MainThread) -> &Self {
        if let Some(control) = self.control.as_deref() {
            let _ = lent(control, |c| c.close_editor(main));
        }
        self
    }

    /// Services the editor; call periodically (about 30 Hz) while it is open.
    ///
    /// Like [`close_editor`](Self::close_editor), a no-op without an editor:
    /// this is driven from a per-frame system that should not branch on a
    /// capability that cannot change after load.
    pub fn editor_idle(&self, main: &MainThread) -> &Self {
        if let Some(control) = self.control.as_deref() {
            control.editor_idle(main);
        }
        self
    }

    /// Asks the plugin to resize its editor and returns the size it applied,
    /// which may be snapped or clamped.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::GuiNotSupported`] when this handle has no editor
    /// route, and otherwise any error of
    /// [`EditorWindow::set_editor_size`](tutti_plugin_types::EditorWindow::set_editor_size),
    /// such as an editor that cannot be resized.
    pub fn set_editor_size(
        &self,
        main: &MainThread,
        requested: EditorSize,
    ) -> Result<EditorSize, EditorError> {
        let control = self.editor_route()?;
        Self::editor_call(control, |c| c.set_editor_size(main, requested))
    }

    /// What this editor supports: resizing, DPI scaling and the rest.
    /// [`EditorCapabilities::default`] when this handle has no editor route.
    pub fn editor_capabilities(&self, main: &MainThread) -> EditorCapabilities {
        self.control
            .as_deref()
            .and_then(|control| lent(control, |c| c.editor_capabilities(main)))
            .unwrap_or_default()
    }

    /// The editor route, or the refusal of a handle with none.
    fn editor_route(&self) -> Result<&dyn MainThreadControl, EditorError> {
        self.control
            .as_deref()
            .ok_or_else(|| EditorError::GuiNotSupported {
                format: self.descriptor.class.format_name().to_string(),
            })
    }

    /// An editor call on the lent control, its failure as the editor's.
    fn editor_call<R>(
        control: &dyn MainThreadControl,
        f: impl FnOnce(&mut dyn Control) -> tutti_plugin_types::PluginResult<R>,
    ) -> Result<R, EditorError> {
        lent(control, f)
            .unwrap_or_else(|| {
                Err(PluginError::Editor(EditorError::PluginError(
                    "the plugin is lost".into(),
                )))
            })
            .map_err(editor_error)
    }

    /// Returns the size the *plugin* has asked its window to become, if it
    /// asked since the last poll.
    ///
    /// Call it beside [`editor_idle`](Self::editor_idle) on the host's frame
    /// loop.
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
    pub fn poll_editor_resize_request(&self, main: &MainThread) -> Option<EditorSize> {
        lent(self.control.as_deref()?, |c| {
            c.poll_editor_resize_request(main)
        })?
    }

    // ---- Automation-state convenience --------------------------------------

    /// Announces the host's [`AutomationMode`] to the plugin so its editor can
    /// update UI feedback.
    ///
    /// # Errors
    ///
    /// Returns [`EditorError::GuiNotSupported`] when this hosting carries no
    /// automation-state advisory (in-process), and otherwise any error of
    /// [`AnyThread::set_automation_mode`].
    pub fn set_automation_mode(&self, mode: AutomationMode) -> Result<(), EditorError> {
        self.any_thread.set_automation_mode(mode)
    }

    // ---- Notify sinks (plugin → host reactions) ----------------------------

    /// Registers a callback invoked when the plugin writes back a
    /// parameter value internally (preset load, automation, host
    /// write-back). Fires on the bridge thread for the out-of-process
    /// backend; on the GUI thread (during `editor_idle`) for the in-process
    /// VST2 backend. Move heavy work to another thread before touching graph
    /// state. Replaces any previous callback.
    pub fn on_parameter_changed<F: Fn(ParamId, Normalized) + Send + Sync + 'static>(
        &self,
        f: F,
    ) -> &Self {
        self.param_sink.set(move |(id, value)| f(id, value));
        self
    }

    /// Registers a callback for **cosmetic** refresh signals: the host's cached
    /// *view* of some plugin state is stale ([`PluginRefresh::ParamValues`] /
    /// [`PluginRefresh::ParamTitles`]) and should be re-read, but the audio graph
    /// is unaffected. See [`Self::on_parameter_changed`] for thread caveats. Only
    /// the out-of-process backend emits these; replaces any previous callback.
    pub fn on_refresh<F: Fn(PluginRefresh) + Send + Sync + 'static>(&self, f: F) -> &Self {
        self.refresh_sink.set(f);
        self
    }

    /// Registers a callback for **structural** invalidation signals: the plugin
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

    /// Removes the parameter-changed callback, if any.
    pub fn clear_parameter_callback(&self) -> &Self {
        self.param_sink.clear();
        self
    }

    /// Removes the refresh callback, if any.
    pub fn clear_refresh_callback(&self) -> &Self {
        self.refresh_sink.clear();
        self
    }

    /// Removes the invalidate callback, if any.
    pub fn clear_invalidate_callback(&self) -> &Self {
        self.invalidate_sink.clear();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_plugin_types::{
        EditorWindow, Normalized, ParamId, ParameterInfo, Preset, PresetId, RenderMode,
    };

    /// A test control that reaches presets. Doubles as proof the route is
    /// implementable from outside this crate's own backends: a `Control` that
    /// is `Clone` is lent as it is.
    #[derive(Clone)]
    struct FakePresets {
        listed: Vec<Preset>,
        accepts: bool,
    }

    impl EditorWindow for FakePresets {
        fn has_editor(&self, _main: &MainThread) -> bool {
            false
        }
        fn open_editor(
            &mut self,
            _main: &MainThread,
            _parent: WindowHandle,
            _scale: ContentScale,
        ) -> tutti_plugin_types::PluginResult<EditorSize> {
            Err(PluginError::Editor(EditorError::PluginError("none".into())))
        }
        fn close_editor(&mut self, _main: &MainThread) {}
        fn editor_idle(&mut self, _main: &MainThread) {}

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
                tutti_plugin_types::EditorError::PluginError(
                    "set_editor_size not supported".into(),
                ),
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

    impl Control for FakePresets {
        fn info(&self) -> &PluginDescriptor {
            unreachable!("the preset tests never ask")
        }
        fn wiring(&self, _main: &MainThread) -> LoadedPlugin {
            LoadedPlugin::default()
        }
        fn parameters(&self, _main: &MainThread) -> Vec<ParameterInfo> {
            Vec::new()
        }
        fn parameter(&self, _main: &MainThread, _id: ParamId) -> Option<Normalized> {
            None
        }
        fn set_parameter(&mut self, _main: &MainThread, _id: ParamId, _value: Normalized) {}
        fn save_state(&mut self, _main: &MainThread) -> tutti_plugin_types::PluginResult<Vec<u8>> {
            Ok(Vec::new())
        }
        fn load_state(
            &mut self,
            _main: &MainThread,
            _bytes: &[u8],
        ) -> tutti_plugin_types::PluginResult<()> {
            Ok(())
        }
        fn presets(&mut self, _main: &MainThread) -> Vec<Preset> {
            self.listed.clone()
        }
        fn load_preset(&mut self, _main: &MainThread, _id: &PresetId) -> bool {
            self.accepts
        }
        fn dropped_reported_edits(&self) -> u64 {
            0
        }
    }

    fn handle_with(presets: Option<FakePresets>) -> PluginHandle {
        handle_with_loaded(presets, LoadedPlugin::default())
    }

    fn handle_with_loaded(presets: Option<FakePresets>, loaded: LoadedPlugin) -> PluginHandle {
        // Only the preset route is under test; the rest of the handle is built
        // from the same fake so no subprocess is needed.
        struct Inert;
        impl AnyThread for Inert {
            fn parameter_descriptors(&self) -> Option<Vec<ParameterInfo>> {
                None
            }
            fn parameter_value(&self, _id: ParamId) -> Option<Normalized> {
                None
            }
            fn set_parameter_value(&self, _id: ParamId, _value: Normalized) {}
            fn parameter_text(&self, _id: ParamId, _value: Normalized) -> Option<String> {
                None
            }
            fn parameter_value_from_text(&self, _id: ParamId, _text: &str) -> Option<Normalized> {
                None
            }
            /// `NoStateRoute`: this backend hosts no plugin, so it has no
            /// state to give and must not answer with an empty one, which a
            /// caller would save as the real thing.
            fn save_state(&self) -> std::result::Result<Vec<u8>, StateError> {
                Err(StateError::NoStateRoute)
            }
            fn set_render_mode(&self, _mode: RenderMode) -> bool {
                false
            }
            fn set_automation_mode(
                &self,
                _mode: AutomationMode,
            ) -> std::result::Result<(), EditorError> {
                Err(EditorError::PluginCrashed)
            }
        }
        impl Health for Inert {
            fn is_crashed(&self) -> bool {
                false
            }
            fn crash_cause(&self) -> Option<String> {
                None
            }
            fn rate_refusal(&self) -> Option<(tutti_types::SampleRate, String)> {
                None
            }
            fn block_counts(&self) -> BlockCounts {
                BlockCounts::default()
            }
            fn terminate(&self, _cause: &str) {}
        }

        PluginHandle::from_backend(
            Arc::new(Inert),
            presets.map(|p| Arc::new(p) as Arc<dyn MainThreadControl>),
            PluginDescriptor::default(),
            loaded,
            ParameterChangeSink::default(),
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
            tutti_plugin_types::Features::PRESET_LIST | tutti_plugin_types::Features::PRESET_LOAD;
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
            Some(FakePresets {
                listed: Vec::new(),
                accepts: true,
            }),
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
            handle_with(None)
                .presets(&MainThread::claim().unwrap())
                .is_none(),
            "a handle with no preset capability must report None, not an empty list"
        );

        let empty = handle_with(Some(FakePresets {
            listed: Vec::new(),
            accepts: false,
        }));
        assert_eq!(
            empty.presets(&MainThread::claim().unwrap()),
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
            handle_with(None)
                .presets(&MainThread::claim().unwrap())
                .is_none(),
            "no route must stay distinct from a refusal, not collapse into one"
        );

        let refusing = handle_with(Some(FakePresets {
            listed: Vec::new(),
            accepts: false,
        }));
        assert_eq!(
            refusing.load_preset(&MainThread::claim().unwrap(), &id),
            Some(false),
            "a refusal must report a present route that said no"
        );

        let accepting = handle_with(Some(FakePresets {
            listed: Vec::new(),
            accepts: true,
        }));
        assert_eq!(
            accepting.load_preset(&MainThread::claim().unwrap(), &id),
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
        let handle = handle_with(Some(FakePresets {
            listed: listed.clone(),
            accepts: true,
        }));
        assert_eq!(
            handle.presets(&MainThread::claim().unwrap()),
            Some(listed),
            "the handle must not reorder or renumber what the plugin listed"
        );
    }
}
