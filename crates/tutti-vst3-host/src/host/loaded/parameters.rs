//! The parameter tree and the controller's optional interfaces: note expression, keyswitches, remaps, representations.

use super::*;
use crate::types::{PlainRange, ProcessContextRequirements, UnitId};
use tutti_plugin_types::AutomationMode;
use tutti_plugin_types::MainThread;
use tutti_plugin_types::ParamUnit;
use tutti_types::UnitParam;

impl Vst3Loaded {
    // VST3 enumerates parameters by index (`getParameterInfo`) and addresses
    // them by ParamID, an opaque plugin-chosen `uint32`. Only the enumeration
    // below touches indices; everything else takes a `ParamId`.

    /// How many parameters the edit controller reports; `0` without one.
    pub fn parameter_count(&self, main: &MainThread) -> u32 {
        self.plugin.parameter_count(main)
    }

    /// The id of every parameter, in the plugin's own order. Empty without an
    /// edit controller.
    pub fn parameter_ids(&self, main: &MainThread) -> Vec<ParamId> {
        (0..self.parameter_count(main))
            .filter_map(|i| self.plugin.parameter_info(main, i))
            .map(|info| ParamId::new(info.id()))
            .collect()
    }

    /// The descriptor of parameter `id`; `None` if the plugin has no such
    /// parameter or no controller. Walks the list, so prefer
    /// [`parameter_list`](Self::parameter_list) for more than one. Each call is
    /// an O(n) walk of FFI calls into the plugin.
    pub fn parameter_info(&self, main: &MainThread, id: ParamId) -> Option<Vst3ParameterInfo> {
        (0..self.parameter_count(main))
            .filter_map(|i| self.plugin.parameter_info(main, i))
            .find(|info| info.id() == id.get())
    }

    /// The descriptor at list position `index`, for the walks that enumerate.
    pub(in crate::host) fn parameter_info_at(
        &self,
        main: &MainThread,
        index: u32,
    ) -> Option<Vst3ParameterInfo> {
        self.plugin.parameter_info(main, index)
    }

    /// Every parameter the plugin advertises, as the SHARED
    /// [`tutti_plugin_types::ParameterInfo`] — the boundary vocabulary every
    /// host crate speaks.
    ///
    /// Each entry carries its descriptor's own `id`; a list position is not an
    /// id, so address a parameter through the entry.
    ///
    /// The unit tree is read **once**, not once per parameter: `units()` is a
    /// round trip into the plugin per unit, and a parameter list is the one
    /// place that cost would multiply.
    ///
    /// A parameter whose descriptor the plugin declines to hand over is
    /// skipped rather than substituted, so the list is short instead of
    /// carrying an invented entry.
    pub fn parameter_list(&self, main: &MainThread) -> Vec<ParameterInfo> {
        let units = self.units(main);
        let groups = unit_names(&units);

        let list = (0..self.parameter_count(main))
            .filter_map(|i| {
                let info = self.parameter_info_at(main, i)?;
                let plain = self.parameter_plain_range(main, ParamId::new(info.id()));
                Some(build_param_info(info, plain, &groups))
            })
            .collect::<Vec<_>>();
        // `IParameterFunctionName` is read once per unit with the function the
        // spec names a mix by: the one role VST3 declares itself.
        let wet = std::iter::once(UnitId::ROOT)
            .chain(units.iter().map(|u| u.id))
            .find_map(|unit| self.param_id_for_function_name(main, unit, DRY_WET_MIX));
        apply_function_roles(list, wet)
    }

    /// The plugin's own `[min, max]` for a parameter, recovered by asking its
    /// controller to invert the normalized map at the endpoints.
    ///
    /// VST3 reports no range on `ParameterInfo` — every value it exchanges is
    /// normalized `0..=1`. But `IEditController::normalizedParamToPlain` is the
    /// same map the plugin's own editor uses to render "440 Hz", so probing it
    /// recovers what the parameter actually means.
    ///
    /// `None` when there is no controller to ask, or when the map is not
    /// monotonic. `normalizedParamToPlain` returns a bare `ParamValue` with no
    /// `tresult`, so a plugin that doesn't implement it cannot report failure —
    /// the midpoint probe is the only available coherence check.
    ///
    /// An identity map is *not* a failure: the SDK's default implementation
    /// returns its input, and for a parameter with no separate plain domain
    /// (a Mix knob) `0..=1` is the truthful answer. That is the difference
    /// between this and hardcoding `0.0..1.0` — the numbers can coincide, but
    /// here they are what the plugin said when asked.
    ///
    /// Must run on the main thread with the controller connected, per the SDK's
    /// `[UI-thread & Connected]` annotation on the method.
    pub fn parameter_plain_range(&self, main: &MainThread, id: ParamId) -> Option<PlainRange> {
        let at = |n: f64| self.plugin.normalized_to_plain(main, id.get(), n);

        let (lo, mid, hi) = (at(0.0)?, at(0.5)?, at(1.0)?);
        if !lo.is_finite() || !mid.is_finite() || !hi.is_finite() {
            return None;
        }
        // A range the endpoints alone would accept but whose interior
        // contradicts them is not a mappable range. Inclusive because a
        // legitimately constant parameter probes flat.
        let monotonic = (lo <= mid && mid <= hi) || (hi <= mid && mid <= lo);
        monotonic.then_some(PlainRange {
            min: lo.min(hi),
            max: lo.max(hi),
        })
    }

    /// Number of per-note expression types the plugin supports on the given
    /// event `bus_index` / MIDI `channel`. Returns `0` if the plugin doesn't
    /// implement `INoteExpressionController`.
    ///
    /// This is the **read** side of note expression — pair it with
    /// [`note_expression_info`](Self::note_expression_info) to enumerate
    /// descriptors. The host can always **send**
    /// [`NoteExpressionValue`](crate::NoteExpressionValue) events regardless of
    /// what this reports.
    pub fn note_expression_count(&self, main: &MainThread, bus_index: i32, channel: i16) -> u32 {
        self.plugin.note_expression_count(main, bus_index, channel)
    }

    /// The `IProcessContextRequirements::getProcessContextRequirements`
    /// flags the plugin returned at load (see [`ProcessContextRequirements`]), or
    /// `None` if it doesn't implement the interface, which a host treats as
    /// "wants everything".
    ///
    /// [`ProcessContextRequirements`]: crate::types::ProcessContextRequirements
    pub fn context_requirements(&self) -> Option<ProcessContextRequirements> {
        requirements(self.plugin.process_context_requirements())
    }

    /// `true` if the plugin asked for any transport field (tempo, playhead,
    /// bar, cycle, time-sig, or transport state). A plugin that implements
    /// `IProcessContextRequirements` and requests none of these does not want a
    /// transport snapshot each block; one that doesn't implement the interface
    /// wants everything.
    pub fn wants_transport(&self) -> bool {
        wants_transport(self.context_requirements())
    }

    /// `true` if the plugin asked for the host-track chord field
    /// (`kNeedChord`) — the VST3 signal that it consumes sequencer context
    /// (chord / scale). Distinct from note-expression.
    pub fn wants_sequencer_context(&self) -> bool {
        wants_sequencer_context(self.context_requirements())
    }

    /// Descriptor for the note-expression type at `index` on the given event
    /// `bus_index` / MIDI `channel` (title, units, value range, flags). Returns
    /// `None` if the index is out of range or the plugin doesn't implement
    /// `INoteExpressionController`.
    pub fn note_expression_info(
        &self,
        main: &MainThread,
        bus_index: i32,
        channel: i16,
        index: u32,
    ) -> Option<Vst3NoteExpressionInfo> {
        self.plugin
            .note_expression_info(main, bus_index, channel, index)
    }

    /// Number of key-switch (articulation) entries the plugin exposes on the
    /// given event `bus_index` / MIDI `channel`. Returns `0` if the plugin
    /// doesn't implement `IKeyswitchController`.
    ///
    /// Pair with [`keyswitch_info`](Self::keyswitch_info) to enumerate the
    /// articulation map a sample-library instrument advertises.
    pub fn keyswitch_count(&self, main: &MainThread, bus_index: i32, channel: i16) -> u32 {
        self.plugin.keyswitch_count(main, bus_index, channel)
    }

    /// Descriptor for the key switch at `index` on the given event `bus_index` /
    /// MIDI `channel` (articulation title, trigger key range, kind). Returns
    /// `None` if the index is out of range or the plugin doesn't implement
    /// `IKeyswitchController`.
    pub fn keyswitch_info(
        &self,
        main: &MainThread,
        bus_index: i32,
        channel: i16,
        index: u32,
    ) -> Option<Vst3KeyswitchInfo> {
        self.plugin.keyswitch_info(main, bus_index, channel, index)
    }

    /// Asks the plugin (via `IRemapParamID`) for the parameter ID in *this*
    /// plugin that corresponds to `old_param_id` from a *previous* plugin
    /// identified by `plugin_to_replace_uid` (its processor class ID / `TUID`).
    ///
    /// This is how a host carries saved automation forward when swapping one
    /// plugin for a newer/compatible one: each old automation lane's `ParamID`
    /// is remapped to the replacement's. Returns `Some(new_id)` when the plugin
    /// reports a compatible parameter (possibly equal to `old_param_id`), or
    /// `None` when there is none or the plugin doesn't implement `IRemapParamID`.
    ///
    /// The host does **not** call this automatically anywhere — like JUCE, it's
    /// exposed for a caller-driven migration flow to use. Must run on the
    /// main/UI thread.
    pub fn remap_param_id(
        &self,
        main: &MainThread,
        plugin_to_replace_uid: &[i8; 16],
        old_param_id: ParamId,
    ) -> Option<ParamId> {
        self.plugin
            .compatible_param_id(main, plugin_to_replace_uid, old_param_id.get())
            .map(ParamId::new)
    }

    /// Resolves a well-known parameter *function name* (the VST3 spec defines
    /// roles like "Wet/Dry Mix", "Master Volume", "Resonance") to the plugin's
    /// `ParamID` for that role, scoped to `unit_id`. Returns `None` if the role
    /// is unknown to the plugin or it doesn't implement `IParameterFunctionName`.
    ///
    /// Lets a host bind a generic "mix" knob to whatever parameter the plugin
    /// uses for it, without hard-coding parameter indices. Main/UI thread.
    pub fn param_id_for_function_name(
        &self,
        main: &MainThread,
        unit_id: crate::UnitId,
        function_name: &str,
    ) -> Option<ParamId> {
        self.plugin
            .param_id_for_function_name(main, unit_id, function_name)
            .map(ParamId::new)
    }

    /// Maps the plugin's physical UI controls to the note-expression dimensions
    /// they drive, on the given event `bus_index` / MIDI `channel`. Returns one
    /// `(PhysicalUiType, note_expression_type)` pair per physical control
    /// (X/Y movement, pressure — see [`PhysicalUiType`](crate::PhysicalUiType)),
    /// or an empty vec if the plugin doesn't implement
    /// `INoteExpressionPhysicalUIMapping`.
    ///
    /// The host allocates the list; the plugin fills the note-expression id each
    /// physical control is wired to. Main/UI thread.
    pub fn physical_ui_mapping(
        &self,
        main: &MainThread,
        bus_index: i32,
        channel: i16,
    ) -> Vec<(crate::PhysicalUiType, u32)> {
        self.plugin.physical_ui_mapping(main, bus_index, channel)
    }

    /// Export the plugin's parameter remote-control layout as XML, for the
    /// representation identified by `(vendor, name, version, host)`. Returns the
    /// XML string, or `None` if the plugin doesn't implement
    /// `IXmlRepresentationController` or produced nothing.
    ///
    /// Hardware controller surfaces use this to lay out a plugin's parameters.
    /// Main/UI thread.
    pub fn xml_representation(
        &self,
        main: &MainThread,
        vendor: &str,
        name: &str,
        version: &str,
        host: &str,
    ) -> Option<String> {
        self.plugin
            .xml_representation(main, vendor, name, version, host)
    }

    /// Tells the plugin the host's current automation read/write mode via
    /// `IAutomationState`.
    ///
    /// Some plugins change behaviour during automation playback vs recording
    /// (e.g. snapping a knob to the automation lane while reading). No-op if the
    /// plugin doesn't implement `IAutomationState`. Returns `true` if the call
    /// was delivered. Must run on the main/UI thread.
    pub fn apply_automation_state(&mut self, main: &MainThread, mode: AutomationMode) -> bool {
        self.plugin
            .set_automation_state(main, tutti_vst3_ffi::AutomationStates::from(mode))
    }
}

/// Whether `req` asks for any transport field; no interface wants everything.
fn wants_transport(req: Option<ProcessContextRequirements>) -> bool {
    use ProcessContextRequirements as f;
    const TRANSPORT_BITS: ProcessContextRequirements = f::NEED_TEMPO
        .union(f::NEED_PROJECT_TIME_MUSIC)
        .union(f::NEED_BAR_POSITION_MUSIC)
        .union(f::NEED_CYCLE_MUSIC)
        .union(f::NEED_TIME_SIGNATURE)
        .union(f::NEED_TRANSPORT_STATE)
        .union(f::NEED_CONTINOUS_TIME_SAMPLES);
    req.is_none_or(|req| req.intersects(TRANSPORT_BITS))
}

/// Whether `req` asks for the chord field; no interface wants everything.
fn wants_sequencer_context(req: Option<ProcessContextRequirements>) -> bool {
    req.is_none_or(|req| req.contains(ProcessContextRequirements::NEED_CHORD))
}

/// `None` for the sentinel the FFI layer keeps for "interface not
/// implemented".
pub(crate) fn requirements(raw: u32) -> Option<ProcessContextRequirements> {
    (raw != u32::MAX).then(|| ProcessContextRequirements::from_bits_retain(raw))
}

/// Index a plugin's unit list by id, keeping only units that can name a group.
///
/// Two exclusions, both deliberate:
///
/// - **The root unit** (`UnitId::ROOT`, id 0) is every plugin's implicit
///   top-level unit, and its name is the plugin's own. Every parameter that
///   declares no unit reports 0, so mapping it would label the entire flat
///   majority with the plugin name — noise on exactly the parameters that have
///   no group.
/// - **Unnamed units.** A unit with an empty name resolves to no group rather
///   than to an empty label, which is the same answer by a shorter route.
pub(super) fn unit_names(units: &[Vst3UnitInfo]) -> HashMap<UnitId, &str> {
    units
        .iter()
        .filter(|u| u.id != UnitId::ROOT && !u.name.is_empty())
        .map(|u| (u.id, u.name.as_str()))
        .collect()
}

/// `FunctionNameType::kDryWetMix`: the function name a plugin gives its
/// dry/wet mix parameter (`ivstparameterfunctionname.h`).
const DRY_WET_MIX: &str = "DryWetMix";

/// Gives the parameter the plugin names as its dry/wet mix the role
/// [`UnitParam::Wet`]. `wet` is what `IParameterFunctionName` answered for
/// [`DRY_WET_MIX`]; `None` (no such interface, or no such function) leaves every
/// role alone.
fn apply_function_roles(mut list: Vec<ParameterInfo>, wet: Option<ParamId>) -> Vec<ParameterInfo> {
    if let Some(wet) = wet {
        for p in list.iter_mut().filter(|p| p.id == wet) {
            p.role = Some(UnitParam::Wet);
        }
    }
    list
}

/// What a VST3 parameter's declaration says it measures (doc 023 §5.2).
///
/// A list (`kIsList`) with at least one step is a [`Choice`](ParamUnit::Choice);
/// otherwise a step count of 1 is a [`Toggle`](ParamUnit::Toggle) (a two-entry
/// list is still a choice, because it names its options); otherwise the `units`
/// string, by [`ParamUnit::from_label`]'s closed list. The range check
/// ([`ParameterInfo::with_measures`]) is the caller's.
fn declared_measures(units: &str, is_list: bool, step_count: i32) -> Option<ParamUnit> {
    if is_list && step_count >= 1 {
        Some(ParamUnit::Choice)
    } else if step_count == 1 {
        Some(ParamUnit::Toggle)
    } else {
        ParamUnit::from_label(units)
    }
}

/// The role a VST3 parameter's flags declare: the bypass switch.
fn declared_role(is_bypass: bool) -> Option<UnitParam> {
    is_bypass.then_some(UnitParam::Bypass)
}

/// Builds a shared [`ParameterInfo`] from a VST3 parameter descriptor and the
/// plain range probed from the plugin's controller.
///
/// `plain` is [`None`] when the plugin has no edit controller to ask, or when
/// its `normalizedParamToPlain` is incoherent — see
/// [`Vst3Loaded::parameter_plain_range`]. The parameter is then `Normalized`,
/// which is what the ABI alone reports.
pub(super) fn build_param_info(
    info: Vst3ParameterInfo,
    plain: Option<PlainRange>,
    groups: &HashMap<UnitId, &str>,
) -> ParameterInfo {
    // VST3 reports every one of these, so all six are known.
    const KNOWN: ParamFlags = ParamFlags::AUTOMATABLE
        .union(ParamFlags::READ_ONLY)
        .union(ParamFlags::WRAP)
        .union(ParamFlags::BYPASS)
        .union(ParamFlags::HIDDEN)
        .union(ParamFlags::PROGRAM_CHANGE);

    let mut reported = ParamFlags::empty();
    reported.set(ParamFlags::AUTOMATABLE, info.can_automate());
    reported.set(ParamFlags::READ_ONLY, info.is_read_only());
    reported.set(ParamFlags::WRAP, info.is_wrap());
    reported.set(ParamFlags::BYPASS, info.is_bypass());
    reported.set(ParamFlags::HIDDEN, info.is_hidden());
    reported.set(ParamFlags::PROGRAM_CHANGE, info.is_program_change());

    // `defaultNormalizedValue` is normalized even when the range is plain, so
    // it goes through the same map as any other incoming value.
    let range = match plain {
        Some(PlainRange { min, max }) => {
            let r = ParamRange::Plain {
                min,
                max,
                default: 0.0,
            };
            ParamRange::Plain {
                min,
                max,
                default: r.to_plain(info.default_normalized_value()),
            }
        }
        None => ParamRange::Normalized {
            default: info.default_normalized_value(),
        },
    };

    // The SDK spells out the encoding at `ivsteditcontroller.h:53`:
    // 0 continuous, 1 toggle, otherwise `max - min` so the position count is
    // one more than the step count.
    let steps = match info.step_count() {
        0 => ParamSteps::Continuous,
        1 => ParamSteps::Toggle,
        n if n > 1 => ParamSteps::Enumerated(n as u32 + 1),
        // Negative is out of contract; report it as unsaid rather than guessing.
        _ => ParamSteps::Unknown,
    };

    let measures = declared_measures(&info.units_string(), info.is_list(), info.step_count());
    let role = declared_role(info.is_bypass());

    ParameterInfo {
        // VST3 `ParamID` — opaque and plugin-chosen, not a list position; see
        // the ParamID-vs-index note above.
        id: info.id().into(),
        name: info.title_string(),
        unit: info.units_string(),
        range,
        steps,
        flags: reported & KNOWN,
        known: KNOWN,
        // A `unitId` naming a unit the plugin never published resolves to no
        // group. Several plugins carry dangling ids — the same class of bug
        // `Vst3UnitInfo::program_list` already absorbs for program lists — and
        // the alternatives are worse: a lookup that panicked would take down a
        // parameter list over a display label, and one that fell back to a
        // neighbour would mislabel silently.
        group: groups
            .get(&info.unit_id())
            .map(|n| n.to_string())
            .unwrap_or_default(),
        measures: None,
        role: None,
    }
    .with_measures(measures)
    .with_role(role)
}

#[cfg(test)]
mod unit_group_tests {
    use super::*;

    /// Builds a unit as the plugin would report it, with no program list.
    fn unit(id: impl Into<UnitId>, name: &str) -> Vst3UnitInfo {
        Vst3UnitInfo {
            id: id.into(),
            parent: Some(UnitId::ROOT),
            name: name.to_string(),
            program_list: None,
            // `kNoProgramListId`: the unit has no list.
            program_list_id_raw: -1,
        }
    }

    /// A parameter naming a published unit is labelled with that unit's name.
    #[test]
    fn a_parameter_takes_the_name_of_the_unit_it_declares() {
        let units = [unit(1, "Filter"), unit(2, "Amp")];
        let groups = unit_names(&units);
        assert_eq!(groups.get(&UnitId::new(1)).copied(), Some("Filter"));
        assert_eq!(groups.get(&UnitId::new(2)).copied(), Some("Amp"));
    }

    /// The root unit names no group.
    ///
    /// This is the case that decides whether grouping is useful or noise:
    /// every parameter that declares no unit reports id 0, so if the root were
    /// mapped, the flat majority of parameters on every plugin would be
    /// labelled with the plugin's own name.
    #[test]
    fn the_root_unit_is_not_a_group() {
        let units = [unit(UnitId::ROOT, "TAL-NoiseMaker"), unit(1, "Filter")];
        let groups = unit_names(&units);
        assert_eq!(
            groups.get(&UnitId::ROOT),
            None,
            "the root unit's name is the plugin's, not a group's"
        );
        assert_eq!(groups.get(&UnitId::new(1)).copied(), Some("Filter"));
    }

    /// A `unitId` naming a unit the plugin never published resolves to no
    /// group, rather than to a neighbour's label.
    ///
    /// Dangling ids are a real plugin bug — the same class
    /// `Vst3UnitInfo::program_list` already absorbs — and the failure mode
    /// worth refusing is a *plausible* wrong answer: a lookup that fell back to
    /// the first unit would silently file the parameter under someone else's
    /// heading.
    #[test]
    fn a_unit_id_that_names_nothing_yields_no_group() {
        let units = [unit(1, "Filter")];
        let groups = unit_names(&units);
        assert_eq!(groups.get(&UnitId::new(7)), None);
    }

    /// A unit the plugin published without a name yields no group, rather than
    /// an empty heading a UI would render as a blank section.
    #[test]
    fn an_unnamed_unit_is_not_a_group() {
        let units = [unit(1, "")];
        let groups = unit_names(&units);
        assert_eq!(groups.get(&UnitId::new(1)), None);
    }
}

#[cfg(test)]
mod requirements_tests {
    use super::{requirements, wants_sequencer_context, wants_transport};
    use crate::types::ProcessContextRequirements as f;

    /// The FFI layer keeps `u32::MAX` for "interface not implemented"; every
    /// other word, including `0` (implemented, asks for nothing), is a real
    /// answer.
    #[test]
    fn only_the_sentinel_means_no_interface() {
        assert_eq!(requirements(u32::MAX), None);
        assert_eq!(requirements(0), Some(f::empty()));
        assert_eq!(requirements(f::NEED_TEMPO.bits()), Some(f::NEED_TEMPO));
    }

    /// **A requirement bit this host does not name survives the decode**, so
    /// the figure a caller reads is what the plugin returned.
    #[test]
    fn an_unnamed_requirement_bit_survives() {
        let raw = f::NEED_TEMPO.bits() | (1 << 30);
        assert_eq!(requirements(raw).map(|r| r.bits()), Some(raw));
    }

    /// A plugin without the interface wants every field; one that has it wants
    /// exactly what it flags.
    #[test]
    fn no_interface_wants_everything() {
        assert!(wants_transport(None));
        assert!(wants_sequencer_context(None));
        assert!(!wants_transport(Some(f::empty())));
        assert!(!wants_sequencer_context(Some(f::empty())));
        assert!(wants_transport(Some(f::NEED_TEMPO)));
        assert!(!wants_sequencer_context(Some(f::NEED_TEMPO)));
        assert!(wants_sequencer_context(Some(f::NEED_CHORD)));
    }
}

#[cfg(test)]
mod measures_tests {
    use super::*;

    fn plain(min: f64, max: f64) -> ParamRange {
        ParamRange::Plain {
            min,
            max,
            default: min,
        }
    }

    /// Builds the `measures` a plugin's declaration ends in, as
    /// `build_param_info` does: the mapping, then the range check.
    fn measures(units: &str, is_list: bool, steps: i32, range: ParamRange) -> Option<ParamUnit> {
        let mut p = ParameterInfo::new(ParamId::new(1), "p");
        p.range = range;
        p.with_measures(declared_measures(units, is_list, steps))
            .measures
    }

    /// Every string on the closed list maps over a plain range that fits.
    #[test]
    fn every_accepted_units_string_maps() {
        for (label, unit) in [
            ("Hz", ParamUnit::Hertz),
            ("dB", ParamUnit::Decibels),
            ("s", ParamUnit::Seconds),
            ("ms", ParamUnit::Milliseconds),
            ("%", ParamUnit::Percent),
            ("ct", ParamUnit::Cents),
            ("st", ParamUnit::Semitones),
            ("BPM", ParamUnit::Bpm),
        ] {
            assert_eq!(
                measures(label, false, 0, plain(1.0, 100.0)),
                Some(unit),
                "{label:?}"
            );
        }
        assert_eq!(
            measures("  Hz ", false, 0, plain(20.0, 20_000.0)),
            Some(ParamUnit::Hertz),
            "ASCII spaces are trimmed"
        );
    }

    /// Near-misses are `None`, whatever the range.
    #[test]
    fn near_misses_are_not_units() {
        for label in ["kHz", "Db", "dB(A)", "semitones", "", "\u{a0}Hz", "hz"] {
            assert_eq!(
                measures(label, false, 0, plain(0.0, 100.0)),
                None,
                "{label:?}"
            );
        }
    }

    /// A label with no plain range, or a range that does not fit, is `None`:
    /// VST3 declares only normalized values, so most plugins get `None` here.
    #[test]
    fn a_unit_without_a_range_that_fits_is_none() {
        let hz = |range| measures("Hz", false, 0, range);
        assert_eq!(hz(ParamRange::Normalized { default: 0.0 }), None);
        assert_eq!(hz(plain(-5.0, 100.0)), None);
        assert_eq!(measures("%", false, 0, plain(0.0, 1000.0)), None);
    }

    /// A list flag with a step count is a `Choice`; a step count of 1 is a
    /// `Toggle`; both over a normalized range, which is all VST3 declares.
    #[test]
    fn a_list_is_a_choice_and_one_step_is_a_toggle() {
        let n = || ParamRange::Normalized { default: 0.0 };
        assert_eq!(measures("", true, 4, n()), Some(ParamUnit::Choice));
        assert_eq!(
            measures("", true, 1, n()),
            Some(ParamUnit::Choice),
            "a two-entry list names its options"
        );
        assert_eq!(measures("", false, 1, n()), Some(ParamUnit::Toggle));
        assert_eq!(measures("", false, 0, n()), None, "continuous, no unit");
        assert_eq!(measures("", true, 0, n()), None, "a list with no steps");
        assert_eq!(measures("", false, 5, n()), None, "stepped, not a list");
    }

    /// The bypass flag is the `Bypass` role, and nothing else gives one.
    #[test]
    fn the_bypass_flag_gives_bypass() {
        assert_eq!(declared_role(true), Some(UnitParam::Bypass));
        assert_eq!(declared_role(false), None);
    }

    /// `IParameterFunctionName`'s answer for the dry/wet mix gives that one
    /// parameter `Wet`; no answer leaves every role alone.
    #[test]
    fn the_dry_wet_function_name_gives_wet() {
        let list = || {
            vec![
                ParameterInfo::new(ParamId::new(1), "Cutoff"),
                ParameterInfo::new(ParamId::new(2), "Mix"),
            ]
        };
        let got = apply_function_roles(list(), Some(ParamId::new(2)));
        assert_eq!(got[0].role, None);
        assert_eq!(got[1].role, Some(UnitParam::Wet));
        let none = apply_function_roles(list(), None);
        assert!(none.iter().all(|p| p.role.is_none()));
        assert_eq!(DRY_WET_MIX, "DryWetMix");
    }
}
