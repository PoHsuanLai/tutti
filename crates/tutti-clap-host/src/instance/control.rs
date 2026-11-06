//! The control surface of a CLAP plugin in the shared vocabulary, implemented
//! by [`ClapLoaded`] and, with parameter writes redirected, by
//! [`ClapController`].

use std::path::Path;
use std::time::Instant;

use tutti_plugin_types::features::probed;
use tutti_plugin_types::{
    ClapFeature, Control, EditorPresence, FdId, Features, Layout, LoadedPlugin, Normalized,
    ParamId, ParamRange, ParameterInfo, PluginClass, PluginDescriptor, PluginError, PluginResult,
    PluginTail, PresetId, Request, RescanScope, Samples, TimerId,
};

use super::controller::ClapController;
use super::lifecycle::plugin_error;
use super::{ClapLoaded, polling::WorkRan};
use crate::types::PluginInfo;

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

/// The inverse of [`normalize`].
fn denormalize(range: Option<(f64, f64)>, value: Normalized) -> f64 {
    match range {
        Some((min, max)) => ParamRange::Plain {
            min,
            max,
            default: min,
        }
        .to_plain(value.get()),
        None => value.get(),
    }
}

impl ClapLoaded {
    /// The descriptor for this plugin as loaded: its factory info, with whether
    /// it has an editor measured from the plugin itself.
    pub(crate) fn measured_descriptor(&self) -> PluginDescriptor {
        let bits = editor_bits(self.has_editor(), self.has_floating_editor());
        descriptor_of(&self.info, EditorPresence::measured(bits.has_editor))
    }

    /// What the plugin declares on its audio ports now: the buses and, per
    /// port, what `clap.surround` says (`None` for a port the plugin will not
    /// answer for: no extension, or no `get_channel_map`).
    pub fn layout(&self) -> Layout {
        let inputs = self.bus_channels(true);
        let outputs = self.bus_channels(false);
        let topology = |is_input: bool, count: usize| {
            (0..count)
                .map(|i| self.surround_topology(is_input, i as u32))
                .collect()
        };
        Layout {
            input_topology: topology(true, inputs.len()),
            output_topology: topology(false, outputs.len()),
            inputs,
            outputs,
        }
    }

    /// Per-bus channel counts for one direction, main bus first, read off the
    /// CLAP `audio-ports` extension (e.g. `[2, 1]` = stereo main + mono
    /// sidechain). Falls back to a single aggregate main bus when the plugin
    /// doesn't implement the extension (empty port list).
    fn bus_channels(&self, is_input: bool) -> tutti_plugin_types::BusChannels {
        let count = self.audio_port_count(is_input);
        let buses: tutti_plugin_types::BusChannels = (0..count)
            .filter_map(|i| self.audio_port_info(i, is_input))
            .map(|p| p.layout)
            .collect();
        if !buses.is_empty() {
            return buses;
        }
        let total = if is_input {
            self.info.audio_inputs
        } else {
            self.info.audio_outputs
        };
        // The aggregate total as a single main bus, the convention the
        // downstream slab expects; none at all stays none.
        match u16::try_from(total) {
            Ok(0) | Err(_) => tutti_plugin_types::BusChannels::new(),
            Ok(n) => vec![tutti_plugin_types::ChannelLayout::from(n)],
        }
    }

    /// The capabilities this plugin has, as the shared vocabulary names them.
    fn features(&self) -> Features {
        let bits = editor_bits(self.has_editor(), self.has_floating_editor());
        let editor_resizable = bits.resize_allowed && self.editor_capabilities().resize.resizable;
        let has_note_in = self.note_port_count(true) > 0;
        let has_note_out = self.note_port_count(false) > 0;

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
    fn tail_figure(&self) -> PluginTail {
        self.tail_samples()
            .map_or(PluginTail::Unknown, PluginTail::from_clap_samples)
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
    ///
    /// # Panics
    ///
    /// In debug builds, panics if called from a thread other than the one that
    /// loaded the plugin.
    pub fn take_requests(&mut self, out: &mut Vec<Request>) {
        self.take_requests_with(Instant::now, out);
    }

    /// Only what the plugin raised since the last take, with no timer or
    /// descriptor polled. For the second look after main-thread work that may
    /// itself have asked for more.
    pub fn take_raised(&self, out: &mut Vec<Request>) {
        out.extend(tutti_plugin_types::taken(&self.host_state.requests));
    }

    /// [`take_requests`](Self::take_requests) with the clock supplied, which it
    /// reads at most once and only when a timer or a descriptor is registered.
    /// Returns which scans it ran.
    pub(crate) fn take_requests_with(
        &mut self,
        clock: impl FnOnce() -> Instant,
        out: &mut Vec<Request>,
    ) -> WorkRan {
        self.assert_main_thread();
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
    fn info(&self) -> &PluginDescriptor {
        self.descriptor.get_or_init(|| self.measured_descriptor())
    }

    /// The wiring read from the plugin now. Latency is read only while the
    /// plugin is active, which is when CLAP lets it change; a loaded plugin
    /// reports none.
    fn wiring(&self) -> LoadedPlugin {
        let layout = self.layout();
        let latency_samples = if self.flags.active {
            Samples(self.latency_samples() as usize)
        } else {
            Samples(0)
        };
        LoadedPlugin {
            inputs: layout.inputs,
            outputs: layout.outputs,
            input_topology: layout.input_topology,
            output_topology: layout.output_topology,
            latency_samples,
            tail: self.tail_figure(),
            features: self.features(),
            probed: probed::CLAP,
        }
    }

    fn parameters(&self) -> Vec<ParameterInfo> {
        self.parameter_list()
    }

    /// Normalized `0..=1`; CLAP has no normalization concept, so the plugin
    /// answers in its plain range and the declared bounds map it back.
    fn parameter(&self, id: ParamId) -> Option<Normalized> {
        let plain = self.plain_value(id.get())?;
        Some(Normalized::new(normalize(
            self.parameter_range(id.get()),
            plain,
        )))
    }

    /// Applied now, as a flushed `PARAM_VALUE` event. CLAP events carry the
    /// plain value, so the declared range denormalizes it.
    fn set_parameter(&mut self, id: ParamId, value: Normalized) {
        let plain = denormalize(self.parameter_range(id.get()), value);
        self.set_plain_value(id.get(), plain);
    }

    /// The plugin's own display string. `value_to_text` takes a plain value, so
    /// the same range conversion `set_parameter` applies is applied here.
    fn parameter_text(&self, id: ParamId, value: Normalized) -> Option<String> {
        let plain = denormalize(self.parameter_range(id.get()), value);
        self.value_to_text(id.get(), plain)
    }

    /// The inverse, re-normalized so the result round-trips into
    /// [`set_parameter`](Self::set_parameter).
    fn parameter_value_from_text(&self, id: ParamId, text: &str) -> Option<Normalized> {
        let plain = self.text_to_value(id.get(), text)?;
        Some(Normalized::new(normalize(
            self.parameter_range(id.get()),
            plain,
        )))
    }

    fn save_state(&mut self) -> PluginResult<Vec<u8>> {
        self.read_state().map_err(plugin_error)
    }

    fn load_state(&mut self, bytes: &[u8]) -> PluginResult<()> {
        self.write_state(bytes).map_err(plugin_error)
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
    fn load_preset(&mut self, id: &PresetId) -> bool {
        match preset_path(id) {
            Some(path) => self.load_preset_file(path).is_ok(),
            None => false,
        }
    }

    /// Nothing is cached here to re-read: the Processor's ranges are the
    /// Controller's to republish.
    fn rescan_parameters(&mut self, scope: RescanScope) {
        let _ = scope;
    }

    fn run_main_thread(&mut self) {
        self.on_main_thread();
    }

    fn run_timer(&mut self, id: TimerId) {
        self.fire_timer(id.0);
    }

    fn service_fd(&mut self, id: FdId) {
        #[cfg(unix)]
        self.fire_fd(id.0);
        #[cfg(not(unix))]
        let _ = id;
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
        Control::info(&**self)
    }

    fn wiring(&self) -> LoadedPlugin {
        Control::wiring(&**self)
    }

    fn parameters(&self) -> Vec<ParameterInfo> {
        Control::parameters(&**self)
    }

    fn parameter(&self, id: ParamId) -> Option<Normalized> {
        Control::parameter(&**self, id)
    }

    /// Queued to the Processor and applied at the head of its next block. A full
    /// ring keeps the latest value per parameter and retries on the next tick.
    fn set_parameter(&mut self, id: ParamId, value: Normalized) {
        self.queue_edit(id, value);
    }

    fn parameter_text(&self, id: ParamId, value: Normalized) -> Option<String> {
        Control::parameter_text(&**self, id, value)
    }

    fn parameter_value_from_text(&self, id: ParamId, text: &str) -> Option<Normalized> {
        Control::parameter_value_from_text(&**self, id, text)
    }

    fn save_state(&mut self) -> PluginResult<Vec<u8>> {
        Control::save_state(&mut **self)
    }

    /// Restores the state, and makes the writes queued before it stale.
    fn load_state(&mut self, bytes: &[u8]) -> PluginResult<()> {
        let result = Control::load_state(&mut **self, bytes);
        self.reset_edits();
        result
    }

    /// Loads the preset, and makes the writes queued before it stale.
    fn load_preset(&mut self, id: &PresetId) -> bool {
        let loaded = Control::load_preset(&mut **self, id);
        if loaded {
            self.reset_edits();
        }
        loaded
    }

    /// An `INFO` or `ALL` rescan changed what the parameters are, so the ranges
    /// the Processor denormalizes against are read again and republished. A
    /// rescan of values or text changes neither.
    fn rescan_parameters(&mut self, scope: RescanScope) {
        if matches!(scope, RescanScope::Info | RescanScope::All) {
            self.republish_ranges();
        }
    }

    fn flush_edits(&mut self) {
        self.retry_edits();
    }

    fn run_main_thread(&mut self) {
        Control::run_main_thread(&mut **self);
    }

    fn run_timer(&mut self, id: TimerId) {
        Control::run_timer(&mut **self, id);
    }

    fn service_fd(&mut self, id: FdId) {
        Control::service_fd(&mut **self, id);
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
    ///
    /// Mutation (run): `preset_path` returning `Some` for `Number` → the first
    /// assertion fails.
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
