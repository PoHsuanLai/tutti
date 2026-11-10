//! Parameter-query and parameter-update methods for [`ClapLoaded`].
//!
//! Values here are **plain**, in each parameter's own declared range — CLAP has
//! no normalized parameter interface. Denormalization against the range table
//! happens on the `process` path, not here.

use super::ClapLoaded;
use crate::events::{ClapEvent, InputEventList, OutputEventList};
use crate::types::{ClapParamFlags, ClapParamInfo};
#[cfg(feature = "clap-extras")]
use crate::types::{Color, ParamAutomationState};
use tutti_plugin_types::MainThread;

/// How a host surface control (e.g. a hardware knob) is bound to a plugin
/// parameter, per `CLAP_EXT_PARAM_INDICATION`. Requires the `clap-extras` feature.
#[cfg(feature = "clap-extras")]
#[derive(Debug, Clone)]
pub struct ParamMapping {
    /// The parameter being bound, by its stable id.
    pub param_id: u32,
    /// Whether a control is bound at all. `false` tells the plugin the
    /// parameter is not mapped to any physical control.
    pub has_mapping: bool,
    /// Colour hint for the control's LED or ring; `None` sends no hint.
    pub color: Option<Color>,
    /// Short label for the control, e.g. the hardware knob's printed name.
    pub label: Option<String>,
    /// Longer description of what the control is bound to.
    pub description: Option<String>,
}

#[cfg(feature = "clap-extras")]
impl ParamMapping {
    /// Creates a mapping entry for the given parameter.
    /// Set `has_mapping = false` to tell the plugin the parameter is no
    /// longer mapped to any physical control.
    #[must_use]
    pub fn new(param_id: u32, has_mapping: bool) -> Self {
        Self {
            param_id,
            has_mapping,
            color: None,
            label: None,
            description: None,
        }
    }

    /// Color hint for the mapped control's LED/ring (builder style).
    #[must_use]
    pub fn color(mut self, color: Color) -> Self {
        self.color = Some(color);
        self
    }

    /// Short label for the mapped control (builder style).
    #[must_use]
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Longer description of the mapping (builder style).
    #[must_use]
    pub fn description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }
}

#[cfg(feature = "clap-extras")]
fn color_to_clap(color: Color) -> clap_sys::color::clap_color {
    clap_sys::color::clap_color {
        alpha: color.alpha,
        red: color.red,
        green: color.green,
        blue: color.blue,
    }
}

impl ClapLoaded {
    /// Number of parameters the plugin exposes. Returns 0 if the plugin
    /// does not implement `CLAP_EXT_PARAMS`.
    pub fn parameter_count(&self, main: &MainThread) -> usize {
        self.plugin.param_count(main) as usize
    }

    /// Current value of a parameter in its own declared range, or `None` if the
    /// plugin does not support the extension or rejects the ID. The normalized
    /// reading is [`Control::parameter`](tutti_plugin_types::Control::parameter).
    pub fn plain_value(&self, main: &MainThread, id: u32) -> Option<f64> {
        self.plugin.param_value(main, id)
    }

    /// Full CLAP-native metadata for the parameter at the given `index`
    /// (0-based, `< parameter_count()`). Private to the crate: the rich native
    /// [`ClapParamInfo`] (with `module` and the full [`ClapParamFlags`]) stays
    /// inside; the crate boundary hands out the shared
    /// [`ParameterInfo`](tutti_plugin_types::ParameterInfo) via
    /// [`parameter_list`](Self::parameter_list).
    pub(crate) fn parameter_info(&self, main: &MainThread, index: u32) -> Option<ClapParamInfo> {
        let info = self.plugin.param_info(main, index)?;
        Some(ClapParamInfo {
            id: info.id,
            name: info.name,
            module: info.module,
            min_value: info.min_value,
            max_value: info.max_value,
            default_value: info.default_value,
            flags: ClapParamFlags::from_bits_truncate(info.flags),
        })
    }

    /// Collects CLAP-native metadata for every parameter. Crate-private (see
    /// [`parameter_info`](Self::parameter_info)).
    ///
    /// A failing `get_info(i)` at `i < count` **truncates**, matching
    /// [`port_channels`](super::load). Parameters are keyed by id downstream so
    /// a skipped entry renumbers nothing — it is worse than that: `activate`
    /// builds the `RangeTable` from this list, and a param missing
    /// from that table takes the pass-through arm in
    /// [`add_param_changes`](crate::events::InputEventList::add_param_changes),
    /// reaching the plugin **un-denormalized** — raw `0..1` into a `100..1100`
    /// Hz range, silently. Scanning past the hole to keep the later ranges is
    /// the tempting alternative and is the worse one: it yields a map complete
    /// for every id but one, so automation looks right everywhere the user
    /// checks. A short list is a visible symptom; a selectively-wrong map is
    /// not.
    pub(crate) fn native_parameters(&self, main: &MainThread) -> Vec<ClapParamInfo> {
        let count = self.parameter_count(main) as u32;
        let mut params = Vec::with_capacity(count as usize);
        for i in 0..count {
            let Some(info) = self.parameter_info(main, i) else {
                // Stop, don't skip: a skipped param is absent from
                // `param_ranges` and its automation arrives un-denormalized.
                break;
            };
            params.push(info);
        }
        params
    }

    /// The declared `[min, max]` for one parameter, or `None` if the plugin
    /// does not declare it.
    ///
    /// The direct `PluginParams` path needs a single parameter's range, where
    /// `process` needs the whole table — so this walks rather than building a
    /// map. That is the right trade here: it is called once per host-driven
    /// read or write (a knob turn, a preset apply), never per block.
    ///
    /// `None` means "no range to convert against", which the caller must treat
    /// as pass-through rather than substituting bounds. Inventing a `0..=1`
    /// default here would silently rescale a parameter the plugin never
    /// described — the same class of quiet wrongness the internal
    /// `parameters` enumeration truncates to avoid.
    pub fn parameter_range(&self, main: &MainThread, id: u32) -> Option<(f64, f64)> {
        let count = self.parameter_count(main) as u32;
        for i in 0..count {
            let info = self.parameter_info(main, i)?;
            if info.id == id {
                return Some((info.min_value, info.max_value));
            }
        }
        None
    }

    /// Every parameter projected onto the shared, format-agnostic
    /// [`ParameterInfo`](tutti_plugin_types::ParameterInfo) — the value that
    /// crosses the crate boundary. CLAP has no unit string, so `unit` is empty;
    /// `step_count` comes from the `STEPPED` flag plus the declared span, since
    /// CLAP reports steppedness as a flag and the count only via `min`/`max`.
    pub fn parameter_list(&self, main: &MainThread) -> Vec<tutti_plugin_types::ParameterInfo> {
        self.native_parameters(main)
            .into_iter()
            .map(project_param_info)
            .collect()
    }

    /// Format a parameter `value` to its human-readable display string via the
    /// plugin's `clap_plugin_params.value_to_text`. Returns `None` if the
    /// plugin does not implement params / value_to_text or declines the id.
    ///
    /// `value` is **plain**, in the parameter's declared range — CLAP has no
    /// normalization concept. `Control::parameter_text` denormalizes against
    /// [`parameter_range`](Self::parameter_range) before calling this, the same
    /// conversion its `set_parameter` applies.
    pub fn value_to_text(&self, main: &MainThread, param_id: u32, value: f64) -> Option<String> {
        self.plugin.param_value_to_text(main, param_id, value)
    }

    /// Parses a display `text` back to a parameter value via the plugin's
    /// `clap_plugin_params.text_to_value`. Returns `None` if the plugin does
    /// not implement params / text_to_value, the string has an interior NUL,
    /// or the plugin cannot parse it.
    ///
    /// The answer is **plain**, in the parameter's declared range — see
    /// [`value_to_text`](Self::value_to_text). `Control` re-normalizes it.
    pub fn text_to_value(&self, main: &MainThread, param_id: u32, text: &str) -> Option<f64> {
        self.plugin.param_text_to_value(main, param_id, text)
    }

    /// Whether the parameter with `param_id` carries the
    /// `CLAP_PARAM_REQUIRES_PROCESS` flag: its changes must be delivered
    /// through `process()`, in event order, and not through an out-of-band
    /// `flush`, while the plugin is processing. `false` if the id is unknown or
    /// the plugin has no params.
    ///
    /// Goes through the same enumeration as `parameter_list`, so the two cannot
    /// drift about which parameters exist.
    pub fn param_requires_process(&self, main: &MainThread, param_id: u32) -> bool {
        self.native_parameters(main)
            .into_iter()
            .find(|info| info.id == param_id)
            .is_some_and(|info| info.flags.contains(ClapParamFlags::REQUIRES_PROCESS))
    }

    /// Delivers parameter changes outside of `process()` via
    /// `clap_plugin_params.flush()`, returning the events the plugin produced in
    /// response.
    ///
    /// Returns an empty `Vec` if the plugin does not implement params or lacks
    /// a flush function.
    ///
    /// # Threading
    /// CLAP declares `params.flush` as `[active ? audio-thread : main-thread]`
    /// and states it "must not be called concurrently to
    /// `clap_plugin->process()`". This gates by state, and on the active path
    /// enforces that with real mutual exclusion, not an assertion:
    ///
    /// - **Inactive** (`!flags.active`) — `activate()` has not run, so a
    ///   main-thread flush is what the spec asks for. This is the GUI-only /
    ///   setup path.
    /// - **Active** (`flags.active`) — the flush runs under an
    ///   [`AudioThreadClaim`](crate::host::AudioThreadClaim), so the calling
    ///   thread *becomes* the audio thread for the duration (which the spec
    ///   explicitly permits for any OS thread) and **blocks** until any
    ///   in-flight `process` on the real audio thread has returned.
    ///
    /// The condition is `active`, **not** `processing`. Those differ for the
    /// whole window between `activate()` and the first `process()` — which is
    /// exactly when a host sets up initial parameter values, and when a plugin
    /// that considers itself active expects the audio-thread role.
    ///
    /// Callers driving an active instance should prefer routing param changes
    /// through the next `process` block (a controller's `set_parameter`, or
    /// `ClapProcessContext::params`) — that is in-order delivery rather than an
    /// out-of-band poke — but doing it here is safe.
    ///
    /// An inactive instance is a main-thread call, which `_main` proves; an
    /// active plugin takes the `[audio-thread]` claim inside the FFI call
    /// (`flush` is an audio-thread entry then).
    pub fn flush_params(
        &mut self,
        main: &MainThread,
        input_events: Vec<ClapEvent>,
    ) -> Vec<ClapEvent> {
        let mut input_list = InputEventList::from_events(input_events);
        input_list.sort_by_time();
        let mut output_list = OutputEventList::new();

        if !self
            .plugin
            .params_flush(main, &input_list, &mut output_list)
        {
            return Vec::new();
        }

        output_list.take_events()
    }

    /// Sets one parameter by flushing a single `PARAM_VALUE` event.
    ///
    /// `value` is plain, in the parameter's declared range. On an active plugin
    /// the flush runs under the `[audio-thread]` claim (see
    /// [`flush_params`](Self::flush_params)); a [`ClapController`](crate::ClapController)
    /// routes its normalized writes through the Processor instead, which delivers
    /// them in order with the block.
    ///
    /// # Panics
    ///
    /// As for [`flush_params`](Self::flush_params).
    pub fn set_plain_value(&mut self, main: &MainThread, id: u32, value: f64) {
        self.flush_params(main, vec![ClapEvent::param_value(0, id, value)]);
    }

    /// Informs the plugin about a host-surface → parameter mapping. No-op if
    /// the plugin does not implement `CLAP_EXT_PARAM_INDICATION`. Requires the `clap-extras` feature.
    #[cfg(feature = "clap-extras")]
    pub fn set_param_mapping(&self, main: &MainThread, mapping: &ParamMapping) {
        self.plugin.set_param_mapping(
            main,
            mapping.param_id,
            mapping.has_mapping,
            mapping.color.map(color_to_clap),
            mapping.label.as_deref(),
            mapping.description.as_deref(),
        );
    }

    /// Informs the plugin of a parameter's automation state so it can update
    /// UI feedback (e.g. knob rings). No-op if the plugin does not implement
    /// `CLAP_EXT_PARAM_INDICATION`. Requires the `clap-extras` feature.
    #[cfg(feature = "clap-extras")]
    pub fn set_param_automation(
        &self,
        main: &MainThread,
        param_id: u32,
        state: ParamAutomationState,
        color: Option<Color>,
    ) {
        self.plugin
            .set_param_automation(main, param_id, state, color.map(color_to_clap));
    }
}

/// Project CLAP's native [`ClapParamInfo`] onto the shared, format-agnostic
/// [`ParameterInfo`](tutti_plugin_types::ParameterInfo). Maps the CLAP flag
/// subset the shared vocabulary models (automatable / read-only / periodic→wrap
/// / bypass / hidden), derives `step_count` from the `STEPPED` bit and the
/// declared span, and leaves `unit` empty (CLAP carries no unit string). CLAP
/// parameter values are in the plugin's native plain range, so
/// `min_value`/`max_value` pass through verbatim.
fn project_param_info(info: ClapParamInfo) -> tutti_plugin_types::ParameterInfo {
    use tutti_plugin_types::{ParamFlags, ParamRange, ParamSteps};

    // CLAP is the one hosted format that reports every capability we model,
    // including the per-voice modulation bits no other format has.
    const KNOWN: ParamFlags = ParamFlags::all();

    let pairs = [
        (ClapParamFlags::AUTOMATABLE, ParamFlags::AUTOMATABLE),
        (ClapParamFlags::READONLY, ParamFlags::READ_ONLY),
        (ClapParamFlags::PERIODIC, ParamFlags::WRAP),
        (ClapParamFlags::BYPASS, ParamFlags::BYPASS),
        (ClapParamFlags::HIDDEN, ParamFlags::HIDDEN),
        (ClapParamFlags::MODULATABLE, ParamFlags::MODULATABLE),
        (
            ClapParamFlags::AUTOMATABLE_PER_NOTE_ID,
            ParamFlags::PER_NOTE_ID,
        ),
        (ClapParamFlags::AUTOMATABLE_PER_KEY, ParamFlags::PER_KEY),
        (
            ClapParamFlags::AUTOMATABLE_PER_CHANNEL,
            ParamFlags::PER_CHANNEL,
        ),
        (ClapParamFlags::AUTOMATABLE_PER_PORT, ParamFlags::PER_PORT),
    ];
    // CLAP splits per-voice targeting across automation and modulation; either
    // one means the host may address that scope.
    let modulation_pairs = [
        (
            ClapParamFlags::MODULATABLE_PER_NOTE_ID,
            ParamFlags::PER_NOTE_ID,
        ),
        (ClapParamFlags::MODULATABLE_PER_KEY, ParamFlags::PER_KEY),
        (
            ClapParamFlags::MODULATABLE_PER_CHANNEL,
            ParamFlags::PER_CHANNEL,
        ),
        (ClapParamFlags::MODULATABLE_PER_PORT, ParamFlags::PER_PORT),
    ];

    let mut flags = ParamFlags::empty();
    for (clap, ours) in pairs.into_iter().chain(modulation_pairs) {
        if info.flags.contains(clap) {
            flags |= ours;
        }
    }

    // CLAP's STEPPED says every value in `[min, max]` is an integer, so the
    // position count comes from the span — not 1. Reporting 1 made an 8-way
    // choice list indistinguishable from a two-state toggle.
    let steps = if info.flags.contains(ClapParamFlags::STEPPED) {
        ParamSteps::from_span(info.max_value - info.min_value)
    } else {
        ParamSteps::Continuous
    };

    tutti_plugin_types::ParameterInfo {
        // `clap_param_info.id` is a `clap_id` — an opaque plugin-chosen handle,
        // the same concept VST3 spells `ParamID` and AU spells
        // `AudioUnitParameterID`. The shared type names all three.
        id: tutti_plugin_types::ParamId::new(info.id),
        name: info.name,
        // CLAP carries no unit string.
        unit: String::new(),
        // CLAP values are in the plugin's native plain range.
        range: ParamRange::Plain {
            min: info.min_value,
            max: info.max_value,
            default: info.default_value,
        },
        steps,
        flags,
        known: KNOWN,
        // `clap_param_info.module` is a `/`-separated path ("Osc 1/Filter"),
        // and CLAP documents it as a display hint rather than an addressing
        // scheme — a host is free to render or ignore it. It passes through
        // verbatim: splitting it here would impose a hierarchy the shared
        // vocabulary does not model, and three of the four hosted formats
        // could not fill.
        group: info.module,
    }
}

#[cfg(test)]
mod grouping_tests {
    use super::*;

    /// CLAP's `module` reaches the shared type as the group, unchanged.
    ///
    /// Kept verbatim rather than split on `/`: CLAP documents the path as a
    /// display hint, and splitting here would build a hierarchy the shared
    /// vocabulary does not model and three of the four hosted formats could
    /// not fill.
    #[test]
    fn a_clap_module_becomes_the_group_verbatim() {
        let mut info = ClapParamInfo::new(1, "Cutoff");
        info.module = "Osc 1/Filter".to_string();

        let projected = project_param_info(info);
        assert_eq!(projected.group, "Osc 1/Filter");
        assert_eq!(projected.qualified_name(), "Osc 1/Filter / Cutoff");
    }

    /// A parameter that declares no module has no group.
    ///
    /// The common case — CLAP leaves `module` empty unless the plugin fills it
    /// — so this is what most parameters must produce.
    #[test]
    fn a_clap_parameter_without_a_module_has_no_group() {
        let projected = project_param_info(ClapParamInfo::new(1, "Cutoff"));
        assert!(projected.group.is_empty());
        assert_eq!(projected.qualified_name(), "Cutoff");
    }
}
