//! The control surface of a CLAP plugin in the shared vocabulary, implemented
//! by [`ClapLoaded`] and, with parameter writes redirected, by
//! [`ClapController`].

use std::path::Path;
use std::time::Instant;
use tutti_plugin_types::MainThread;

use tutti_plugin_types::features::probed;
use tutti_plugin_types::{
    BusRole, ChannelLayout, ClapFeature, ContentScale, Control, EditorPresence, EditorSize,
    EditorWindow, FdId, Features, Layout, LoadedPlugin, Normalized, ParamId, ParamRange,
    ParameterInfo, PluginBus, PluginBuses, PluginClass, PluginDescriptor, PluginError,
    PluginResult, PluginTail, PresetId, RenderMode, Request, RescanScope, Samples, TimerId,
    WindowHandle,
};

use super::controller::ClapController;
use super::lifecycle::plugin_error;
use crate::events::ParamBounds;
use super::{ClapLoaded, polling::WorkRan};
use crate::types::{AudioPortFlags, PluginInfo};

/// What the two editor answers mean for the three capability bits.
#[derive(Debug, PartialEq, Eq)]
struct EditorBits {
    /// `Features::EDITOR` — is there a UI at all?
    has_editor: bool,
    /// Whether `Features::EDITOR_RESIZE` may be set, subject to the plugin's
    /// own resize hints.
    resize_allowed: bool,
    /// `Features::EDITOR_FLOATING` — does the plugin own the window?
    floating: bool,
}

/// Resolve the two editor answers into the three bits that depend on them.
///
/// CLAP asks about editors twice — once for embedded, once for floating — and
/// each bit wants a different combination:
///
/// - **`Features::EDITOR` is the union.** It means "this plugin has a UI", and
///   a floating-only plugin has one. Reporting only the embedded answer is what
///   made such a plugin look editor-less, so the DAW drew no button for a UI it
///   could have shown.
/// - **`Features::EDITOR_RESIZE` is embedded-only.** `can_resize`,
///   `adjust_size` and `set_size` are all marked `[main-thread & !floating]` in
///   `ext/gui.h`; a floating window is the plugin's to size. Advertising a
///   resize path for one would announce a capability nothing can drive.
/// - **`Features::EDITOR_FLOATING` is "embedding is not available".** Set only
///   when the plugin floats *and cannot embed* — a plugin supporting both is
///   hosted embedded, because that gives the host control over placement and
///   keeps the editor inside the session's window management. Floating is the
///   fallback for plugins with no choice, not a preference to honour.
///
/// That last rule is why `prefers_floating()` is not consulted here.
/// `ext/gui.h:113` calls the preference a hint the host has no obligation to
/// honour, and a host that floated every plugin asking to would scatter windows
/// it can no longer place, size or stack.
fn editor_bits(embeddable: bool, floating: bool) -> EditorBits {
    EditorBits {
        has_editor: embeddable || floating,
        resize_allowed: embeddable,
        floating: floating && !embeddable,
    }
}

/// The catalog descriptor from CLAP factory info, carrying its feature tags
/// verbatim as the native class.
pub fn descriptor_of(info: &PluginInfo, editor: EditorPresence) -> PluginDescriptor {
    PluginDescriptor {
        id: info.id.clone(),
        name: info.name.clone(),
        vendor: info.vendor.clone(),
        version: info.version.clone(),
        class: PluginClass::Clap {
            features: info
                .features
                .iter()
                .map(|f| ClapFeature::parse(f))
                .collect(),
        },
        editor,
    }
}

/// `plain` against the parameter's declared range, `0..=1`. A parameter the
/// plugin declares no range for is passed through rather than scaled against
/// invented bounds, the same choice `add_param_changes` makes for an id missing
/// from its range table.
fn normalize(range: Option<(f64, f64)>, plain: f64) -> f64 {
    match range {
        Some((min, max)) => ParamRange::Plain {
            min,
            max,
            default: min,
        }
        .to_normalized(plain),
        None => plain,
    }
}

/// The inverse of [`normalize`]: the plain value, in `f64`, with a stepped
/// parameter on its integer step.
fn denormalize(bounds: Option<ParamBounds>, value: Normalized) -> f64 {
    match bounds {
        Some(bounds) => bounds.plain(value.get()),
        None => value.get(),
    }
}

impl ClapLoaded {
    /// The descriptor for this plugin as loaded: its factory info, with whether
    /// it has an editor measured from the plugin itself.
    pub(crate) fn measured_descriptor(&self, main: &MainThread) -> PluginDescriptor {
        let bits = editor_bits(self.has_editor(main), self.has_floating_editor(main));
        descriptor_of(&self.info, EditorPresence::measured(bits.has_editor))
    }

    /// What the plugin declares on its audio ports now: per port, its width,
    /// what `clap.surround` says (`None` for a port the plugin will not answer
    /// for: no extension, or no `get_channel_map`) and its role.
    pub fn layout(&self, main: &MainThread) -> Layout {
        Layout {
            inputs: self.buses(main, true),
            outputs: self.buses(main, false),
        }
    }

    /// One direction's buses, read off the CLAP `audio-ports` extension (e.g.
    /// a stereo main and a mono aux). A port flagged
    /// [`MAIN`](AudioPortFlags::MAIN) is [`BusRole::Main`], any other
    /// [`BusRole::Aux`]: CLAP marks no port a sidechain. The list stops at
    /// the first index `get` refuses, as the port table does, so a bus here is
    /// the port at its index. A plugin without the extension (no ports) is
    /// read as its aggregate total on a single main bus; none at all stays
    /// none.
    fn buses(&self, main: &MainThread, is_input: bool) -> PluginBuses {
        let count = self.audio_port_count(main, is_input);
        let buses: PluginBuses = (0..count)
            .map_while(|i| {
                let port = self.audio_port_info(main, i, is_input)?;
                let role = if port.flags.contains(AudioPortFlags::MAIN) {
                    BusRole::Main
                } else {
                    BusRole::Aux
                };
                Some(
                    PluginBus::new(port.layout, role)
                        .placed(self.surround_topology(main, is_input, i)),
                )
            })
            .collect();
        if !buses.is_empty() {
            return buses;
        }
        let total = if is_input {
            self.info.audio_inputs
        } else {
            self.info.audio_outputs
        };
        match u16::try_from(total) {
            Ok(0) | Err(_) => PluginBuses::new(),
            Ok(n) => vec![PluginBus::main(ChannelLayout::from(n))],
        }
    }

    /// The capabilities this plugin has, as the shared vocabulary names them.
    fn features(&self, main: &MainThread) -> Features {
        let bits = editor_bits(self.has_editor(main), self.has_floating_editor(main));
        let editor_resizable =
            bits.resize_allowed && self.editor_capabilities(main).resize.resizable;
        let has_note_in = self.note_port_count(main, true) > 0;
        let has_note_out = self.note_port_count(main, false) > 0;

        let mut features = Features::empty();
        features.set(Features::MIDI_IN, has_note_in);
        features.set(Features::MIDI_OUT, has_note_out);
        features.set(Features::EDITOR, bits.has_editor);
        features.set(Features::EDITOR_RESIZE, editor_resizable);
        features.set(Features::EDITOR_FLOATING, bits.floating);
        // CLAP always carries transport, sample-accurate param automation, and
        // the full note-expression dimension set (see build_clap_transport /
        // PARAM_VALUE events / CLAP_EVENT_NOTE_EXPRESSION). No sequencer context
        // (the CLAP spec defines no chord/scale events).
        features.insert(Features::TRANSPORT);
        features.insert(Features::PARAM_AUTOMATION);
        features.set(Features::NOTE_EXPRESSION, has_note_in);
        // `PRESET_LIST` stays out of `probed::CLAP`: enumeration lives in the
        // preset-discovery extension, which is factory-level and unbound here,
        // so only the load half is answered.
        features.set(Features::PRESET_LOAD, self.supports_preset_load());
        // The format the plugin runs at: 64-bit only while activated as such.
        features.set(Features::F64_AUDIO, self.audio.f64_active);
        features
    }

    /// The tail figure; a plugin without `clap.tail` has not answered, which is
    /// not the `None` a plugin answering 0 gives.
    fn tail_figure(&self, main: &MainThread) -> PluginTail {
        self.tail_samples(main)
            .map_or(PluginTail::Unknown, PluginTail::from_clap_samples)
    }

    /// Only what the plugin raised since the last take, with no timer or
    /// descriptor polled. For the second look after main-thread work that may
    /// itself have asked for more.
    pub fn take_raised(&self, out: &mut Vec<Request>) {
        out.extend(tutti_plugin_types::taken(&self.host_state.requests));
    }

    /// [`Control::take_requests`] with the clock supplied, which it
    /// reads at most once and only when a timer or a descriptor is registered.
    /// Returns which scans it ran.
    pub(crate) fn take_requests_with(
        &mut self,
        _main: &MainThread,
        clock: impl FnOnce() -> Instant,
        out: &mut Vec<Request>,
    ) -> WorkRan {
        out.extend(tutti_plugin_types::taken(&self.host_state.requests));
        let (ran, due_timers) = self.due_work(clock);
        out.extend(due_timers.into_iter().map(|id| Request::Timer(TimerId(id))));
        #[cfg(unix)]
        if ran.fds {
            out.extend(
                self.work
                    .ready_fds()
                    .iter()
                    .map(|&(fd, _)| Request::Fd(FdId(fd))),
            );
        }
        ran
    }
}

impl Control for ClapLoaded {
    /// The editor is measured from the plugin on the first ask, which only the
    /// main thread may make; any other thread gets the unmeasured descriptor
    /// ([`EditorPresence::Unknown`]) and measures nothing.
    fn info(&self) -> &PluginDescriptor {
        if let Some(measured) = self.descriptor.get() {
            return measured;
        }
        match MainThread::current() {
            Some(main) => self
                .descriptor
                .get_or_init(|| self.measured_descriptor(&main)),
            None => &self.unmeasured,
        }
    }

    /// The wiring read from the plugin now. Latency is read only while the
    /// plugin is active, which is when CLAP lets it change; a loaded plugin
    /// reports none.
    fn wiring(&self, main: &MainThread) -> LoadedPlugin {
        let layout = self.layout(main);
        let latency_samples = if self.flags.active {
            Samples(self.latency_samples(main) as usize)
        } else {
            Samples(0)
        };
        LoadedPlugin {
            inputs: layout.inputs,
            outputs: layout.outputs,
            latency_samples,
            tail: self.tail_figure(main),
            features: self.features(main),
            probed: probed::CLAP,
        }
    }

    fn parameters(&self, main: &MainThread) -> Vec<ParameterInfo> {
        self.parameter_list(main)
    }

    fn parameter_count(&self, main: &MainThread) -> usize {
        ClapLoaded::parameter_count(self, main)
    }

    /// Normalized `0..=1`; CLAP has no normalization concept, so the plugin
    /// answers in its plain range and the declared bounds map it back.
    fn parameter(&self, main: &MainThread, id: ParamId) -> Option<Normalized> {
        let plain = self.plain_value(main, id.get())?;
        Some(Normalized::clamped(normalize(
            self.parameter_range(main, id.get()),
            plain,
        )))
    }

    /// Applied now, as a flushed `PARAM_VALUE` event. CLAP events carry the
    /// plain value, so the declared range denormalizes it.
    fn set_parameter(&mut self, main: &MainThread, id: ParamId, value: Normalized) {
        let plain = denormalize(self.parameter_bounds(main, id.get()), value);
        self.set_plain_value(main, id.get(), plain);
    }

    /// The plugin's own display string. `value_to_text` takes a plain value, so
    /// the same range conversion `set_parameter` applies is applied here.
    fn parameter_text(&self, main: &MainThread, id: ParamId, value: Normalized) -> Option<String> {
        let plain = denormalize(self.parameter_bounds(main, id.get()), value);
        self.value_to_text(main, id.get(), plain)
    }

    /// The inverse, re-normalized so the result round-trips into
    /// [`set_parameter`](Self::set_parameter).
    fn parameter_value_from_text(
        &self,
        main: &MainThread,
        id: ParamId,
        text: &str,
    ) -> Option<Normalized> {
        let plain = self.text_to_value(main, id.get(), text)?;
        Some(Normalized::clamped(normalize(
            self.parameter_range(main, id.get()),
            plain,
        )))
    }

    fn save_state(&mut self, main: &MainThread) -> PluginResult<Vec<u8>> {
        self.read_state(main).map_err(plugin_error)
    }

    fn load_state(&mut self, main: &MainThread, bytes: &[u8]) -> PluginResult<()> {
        self.write_state(main, bytes).map_err(plugin_error)
    }

    /// Loads the preset a [`PresetId::Location`] names, via
    /// `CLAP_EXT_PRESET_LOAD`.
    ///
    /// That is the only shape CLAP can address: the extension's
    /// `from_location` takes a filesystem path and nothing else. A `Number` or
    /// `Program` names no CLAP preset and is refused rather than coerced; there
    /// is no number to coerce it *to*, which is why the id is opaque.
    ///
    /// CLAP cannot enumerate: discovery lives in the factory-level
    /// preset-discovery extension, which this host does not bind, so `presets`
    /// keeps its empty default. That is not the same claim as an empty list from
    /// AU: `Features::PRESET_LIST` is *unprobed* for CLAP.
    fn load_preset(&mut self, main: &MainThread, id: &PresetId) -> bool {
        match preset_path(id) {
            Some(path) => self.load_preset_file(main, path).is_ok(),
            None => false,
        }
    }

    /// Nothing is cached here to re-read: the Processor's ranges are the
    /// Controller's to republish.
    fn rescan_parameters(&mut self, _main: &MainThread, scope: RescanScope) {
        let _ = scope;
    }

    fn run_main_thread(&mut self, main: &MainThread) {
        self.on_main_thread(main);
    }

    fn run_timer(&mut self, main: &MainThread, id: TimerId) {
        self.fire_timer(main, id.0);
    }

    fn service_fd(&mut self, main: &MainThread, id: FdId) {
        #[cfg(unix)]
        self.fire_fd(main, id.0);
        #[cfg(not(unix))]
        let _ = (main, id);
    }

    /// Everything one main-thread pass turns into the core's requests: what
    /// the plugin asked for from any thread, the timers that are due and the
    /// descriptors that are ready. Appended to `out`; call it between blocks on
    /// the thread that loaded the plugin, as often as the host ticks (a timer's
    /// period is at best that fine).
    ///
    /// It is cheap when there is nothing to do, so it can run on every pass of
    /// a real-time-priority thread: the word is one atomic, and the clock is
    /// read only while a timer or a descriptor is registered. The timers lock
    /// is taken only once the earliest deadline has passed, and the descriptors
    /// are polled at most once per 5 ms.
    ///
    /// Timers and callbacks are served only while the host keeps calling this
    /// and feeding the requests to the core: a loop that blocks waiting for the
    /// next message serves none until one arrives.
    fn take_requests(&mut self, main: &MainThread, out: &mut Vec<Request>) {
        self.take_requests_with(main, Instant::now, out);
    }

    /// Switches between real-time and offline rendering per `CLAP_EXT_RENDER`.
    /// `true` when the plugin accepted, `false` when it has no `clap.render` or
    /// refused (the next activation carries [`Prepare::mode`](tutti_plugin_types::Prepare::mode)
    /// either way).
    fn set_render_mode(&mut self, main: &MainThread, mode: RenderMode) -> bool {
        self.plugin.set_render_offline(main, mode.is_offline())
    }

    /// Nothing: CLAP reports the edits a plugin's own editor makes in the
    /// block's `ProcessOutput`.
    fn take_reported_edits(&mut self, _main: &MainThread, _out: &mut Vec<(ParamId, Normalized)>) {}

    /// `0`: CLAP reports the edits a plugin's own editor makes in the block's
    /// `ProcessOutput`, not through a queue that can drop them.
    fn dropped_reported_edits(&self) -> u64 {
        0
    }
}

impl EditorWindow for ClapLoaded {
    /// Whether [`open_editor`](EditorWindow::open_editor) can actually embed an editor.
    ///
    /// Not just "is there a `clap.gui` vtable?": a plugin may expose the
    /// extension but omit `create`, or support only a floating window. Both are
    /// legal CLAP and both make `embed_editor_sequence` fail, so both answer
    /// `false` here.
    ///
    /// **This is the embedded question only, and it is not "has an editor".**
    /// A floating-only plugin has a perfectly good editor that this returns
    /// `false` for — ask [`has_floating_editor`](Self::has_floating_editor) for
    /// the other half. A caller deciding whether to show a UI at all wants the
    /// union; one deciding whether it can *embed* wants this. Feeding only this
    /// into `Features::EDITOR` is what made floating-only plugins report no
    /// editor at all.
    ///
    /// Deliberately does not create a GUI to find out. `is_api_supported` is a
    /// side-effect-free predicate; `create` allocates the plugin's real GUI
    /// resources. So a plugin whose `is_api_supported` says yes and whose
    /// `create` then fails still reports `true` here; that surfaces as
    /// [`open_editor`](EditorWindow::open_editor)'s error. An absent `is_api_supported`
    /// is not a refusal — the same reading `embed_editor_sequence` uses.
    fn has_editor(&self, main: &MainThread) -> bool {
        self.plugin.has_editor(main)
    }

    /// Creates the plugin editor and embeds it into the given native `parent`
    /// window, whose backing-store scale is `scale`, returning the editor's
    /// initial size.
    ///
    /// Follows the CLAP embed sequence: `is_api_supported` → `create` →
    /// `set_scale(scale)` → `get_size` → `set_parent` → `show`. See
    /// `embed_editor_sequence` for the ordering rationale and for which
    /// window apis take the `set_scale` step (win32 and x11; never cocoa).
    ///
    /// # Errors
    /// [`PluginError::Editor`] if the plugin does not expose a GUI, if the
    /// embedded window API is unsupported, or if `create`/`set_parent` fails.
    fn open_editor(
        &mut self,
        main: &MainThread,
        parent: WindowHandle,
        scale: ContentScale,
    ) -> PluginResult<EditorSize> {
        self.embed_editor(main, parent, scale).map_err(plugin_error)
    }

    /// Opens the plugin's own floating window with no transient parent, titled
    /// with the plugin's name: this crate does not hold the host's window, and
    /// `set_transient` is a stacking hint the plugin may ignore anyway. A host
    /// that has both uses
    /// [`open_floating_editor_with`](ClapLoaded::open_floating_editor_with).
    fn open_floating_editor(&mut self, main: &MainThread) -> PluginResult<()> {
        let title = std::ffi::CString::new(self.info().name.as_str())
            .unwrap_or_else(|_| c"Plugin Editor".to_owned());
        self.open_floating_editor_with(main, None, &title)
            .map_err(plugin_error)
    }

    /// Hides and destroys the plugin editor, if one was opened. Idempotent.
    ///
    /// `gui.hide` is skipped when the plugin reported
    /// `gui.closed(was_destroyed = true)`: that says its window is gone, and
    /// `hide` acts on a window. `gui.destroy` runs either way —
    /// `ext/gui.h` states the host "must call clap_plugin_gui->destroy() to
    /// acknowledge the gui destruction", and the spec's own lifecycle pairs
    /// `destroy` (step 14) with `create` (step 2), so it releases the gui
    /// resources `create` allocated rather than the window that just closed.
    fn close_editor(&mut self, main: &MainThread) {
        self.destroy_editor(main);
    }
    /// Nothing, on purpose: CLAP's editor timers, callbacks and file
    /// descriptors are served by [`take_requests`](Control::take_requests),
    /// whose requests the shell feeds to `Lifecycle::drive` (`run_timer`,
    /// `run_main_thread`, `service_fd`). A second pass here would take
    /// requests that belong to it.
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
            tutti_plugin_types::EditorError::PluginError("set_editor_size not supported".into()),
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

/// The filesystem path an id names, or `None` when it names none.
///
/// A free function so the decision is testable on its own. Through
/// `load_preset` it is not: a coerced non-path id produces a path that does not
/// exist, the plugin refuses *that*, and the caller sees the same `false` the
/// guard would have produced. The two refusals are indistinguishable from
/// outside, so only the guard's own inputs can pin it.
fn preset_path(id: &PresetId) -> Option<&Path> {
    match id {
        PresetId::Location(p) => Some(p.as_path()),
        PresetId::Number(_) | PresetId::Program { .. } => None,
    }
}

impl Control for ClapController {
    fn info(&self) -> &PluginDescriptor {
        Control::info(self.loaded())
    }

    fn wiring(&self, main: &MainThread) -> LoadedPlugin {
        Control::wiring(self.loaded(), main)
    }

    fn parameters(&self, main: &MainThread) -> Vec<ParameterInfo> {
        Control::parameters(self.loaded(), main)
    }

    fn parameter_count(&self, main: &MainThread) -> usize {
        Control::parameter_count(self.loaded(), main)
    }

    fn parameter(&self, main: &MainThread, id: ParamId) -> Option<Normalized> {
        Control::parameter(self.loaded(), main, id)
    }

    /// Queued to the Processor and applied at the head of its next block. A full
    /// ring keeps the latest value per parameter and retries on the next tick.
    fn set_parameter(&mut self, _main: &MainThread, id: ParamId, value: Normalized) {
        self.queue_edit(id, value);
    }

    fn parameter_text(&self, main: &MainThread, id: ParamId, value: Normalized) -> Option<String> {
        Control::parameter_text(self.loaded(), main, id, value)
    }

    fn parameter_value_from_text(
        &self,
        main: &MainThread,
        id: ParamId,
        text: &str,
    ) -> Option<Normalized> {
        Control::parameter_value_from_text(self.loaded(), main, id, text)
    }

    fn save_state(&mut self, main: &MainThread) -> PluginResult<Vec<u8>> {
        Control::save_state(self.loaded_mut(), main)
    }

    /// Restores the state, and makes the writes queued before it stale.
    fn load_state(&mut self, main: &MainThread, bytes: &[u8]) -> PluginResult<()> {
        let result = Control::load_state(self.loaded_mut(), main, bytes);
        self.reset_edits();
        result
    }

    /// Loads the preset, and makes the writes queued before it stale.
    fn load_preset(&mut self, main: &MainThread, id: &PresetId) -> bool {
        let loaded = Control::load_preset(self.loaded_mut(), main, id);
        if loaded {
            self.reset_edits();
        }
        loaded
    }

    /// An `INFO` or `ALL` rescan changed what the parameters are, so the ranges
    /// the Processor denormalizes against are read again and republished. A
    /// rescan of values or text changes neither.
    fn rescan_parameters(&mut self, main: &MainThread, scope: RescanScope) {
        if matches!(scope, RescanScope::Info | RescanScope::All) {
            self.republish_ranges(main);
        }
    }

    fn flush_edits(&mut self, _main: &MainThread) {
        self.retry_edits();
    }

    fn run_main_thread(&mut self, main: &MainThread) {
        Control::run_main_thread(self.loaded_mut(), main);
    }

    fn run_timer(&mut self, main: &MainThread, id: TimerId) {
        Control::run_timer(self.loaded_mut(), main, id);
    }

    fn service_fd(&mut self, main: &MainThread, id: FdId) {
        Control::service_fd(self.loaded_mut(), main, id);
    }

    fn take_requests(&mut self, main: &MainThread, out: &mut Vec<Request>) {
        Control::take_requests(self.loaded_mut(), main, out);
    }

    fn set_render_mode(&mut self, main: &MainThread, mode: RenderMode) -> bool {
        Control::set_render_mode(self.loaded_mut(), main, mode)
    }

    fn take_reported_edits(&mut self, main: &MainThread, out: &mut Vec<(ParamId, Normalized)>) {
        Control::take_reported_edits(self.loaded_mut(), main, out);
    }

    fn dropped_reported_edits(&self) -> u64 {
        Control::dropped_reported_edits(self.loaded())
    }
}

impl EditorWindow for ClapController {
    fn has_editor(&self, main: &MainThread) -> bool {
        EditorWindow::has_editor(self.loaded(), main)
    }

    fn open_editor(
        &mut self,
        main: &MainThread,
        parent: WindowHandle,
        scale: ContentScale,
    ) -> PluginResult<EditorSize> {
        EditorWindow::open_editor(self.loaded_mut(), main, parent, scale)
    }

    fn open_floating_editor(&mut self, main: &MainThread) -> PluginResult<()> {
        EditorWindow::open_floating_editor(self.loaded_mut(), main)
    }

    fn close_editor(&mut self, main: &MainThread) {
        EditorWindow::close_editor(self.loaded_mut(), main);
    }

    /// Nothing, as the Loaded's is: see [`ClapLoaded`]'s `editor_idle`.
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
            tutti_plugin_types::EditorError::PluginError("set_editor_size not supported".into()),
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

#[allow(dead_code)]
fn _error_is_used(_: PluginError) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// A floating-only plugin reports an editor.
    ///
    /// The user-visible half of the floating-window gap: `Features::EDITOR`
    /// drives whether the DAW offers to open a UI at all, and keying it on the
    /// embedded answer alone hid every floating-only plugin's editor behind a
    /// button that was never drawn.
    #[test]
    fn a_floating_only_plugin_reports_an_editor() {
        assert!(
            editor_bits(false, true).has_editor,
            "floating-only means the plugin has a UI, just not an embeddable one"
        );
    }

    /// ...but does not advertise resize.
    ///
    /// The pair that keeps the union from being applied to both bits. Every
    /// resize entry point is `[!floating]`, so a host acting on this bit for a
    /// floating window would be calling what the spec forbids.
    #[test]
    fn a_floating_only_plugin_does_not_advertise_resize() {
        assert!(
            !editor_bits(false, true).resize_allowed,
            "can_resize/adjust_size/set_size are all !floating — a floating \
             window is the plugin's to size"
        );
    }

    /// Floating is reported only when embedding is *unavailable*.
    ///
    /// The rule that keeps the host in control of its own window management: a
    /// plugin supporting both is embedded, so the session can place, size and
    /// stack it. Reading `EDITOR_FLOATING` as "the plugin would like to float"
    /// instead would scatter windows the host can no longer manage — and
    /// `ext/gui.h:113` explicitly calls the preference a hint the host need not
    /// honour.
    #[test]
    fn floating_is_reported_only_when_embedding_is_unavailable() {
        assert!(
            editor_bits(false, true).floating,
            "floating-only: nothing else is possible, so the host must float it"
        );
        assert!(
            !editor_bits(true, true).floating,
            "supports both: embed it, because that is the mode the host can manage"
        );
        assert!(
            !editor_bits(true, false).floating,
            "embed-only is not floating"
        );
        assert!(
            !editor_bits(false, false).floating,
            "no editor at all is not a floating editor"
        );
    }

    /// An embeddable plugin gets both bits, and a plugin with neither gets none.
    ///
    /// The two ends. Without the second, `editor_bits` returning `(true, true)`
    /// unconditionally would satisfy every other case here.
    #[test]
    fn editor_bits_track_the_plugin() {
        let embed_only = editor_bits(true, false);
        assert!(embed_only.has_editor && embed_only.resize_allowed);

        let both = editor_bits(true, true);
        assert!(
            both.has_editor && both.resize_allowed,
            "supporting both is still embeddable, so resize stays available"
        );

        let neither = editor_bits(false, false);
        assert!(
            !neither.has_editor && !neither.resize_allowed,
            "no editor either way means no editor — this is the case \
             `Features::EDITOR` must still be able to report"
        );
    }

    /// Only a filesystem location names a CLAP preset: the other ids address
    /// nothing here, and are refused rather than coerced.
    ///
    /// Asserted on the guard, not through `load_preset`: a coerced id yields a
    /// nonexistent path the plugin refuses anyway, so the two refusals look
    /// identical from outside.
    #[test]
    fn only_a_location_is_a_clap_preset() {
        assert_eq!(
            preset_path(&PresetId::Number(0)),
            None,
            "an AU selector / VST2 index addresses no CLAP preset"
        );
        assert_eq!(
            preset_path(&PresetId::Program {
                list_id: 0,
                index: 0
            }),
            None,
            "a VST3 program id addresses no CLAP preset"
        );
        assert_eq!(
            preset_path(&PresetId::Location("/x.clap-preset".into())),
            Some(Path::new("/x.clap-preset")),
            "a path-shaped id reaches the plugin verbatim"
        );
    }
}
