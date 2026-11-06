//! The parameter tree and the controller's optional interfaces: note expression, keyswitches, remaps, representations.

use super::*;
use crate::types::PlainRange;
use tutti_plugin_types::AutomationMode;

impl Vst3Loaded {
    // VST3 enumerates parameters by index (`getParameterInfo`) and addresses
    // them by ParamID, an opaque plugin-chosen `uint32`. Only the enumeration
    // below touches indices; everything else takes a `ParamId`.

    /// How many parameters the edit controller reports; `0` without one.
    pub fn parameter_count(&self) -> u32 {
        self.plugin.parameter_count()
    }

    /// The id of every parameter, in the plugin's own order. Empty without an
    /// edit controller.
    pub fn parameter_ids(&self) -> Vec<ParamId> {
        (0..self.parameter_count())
            .filter_map(|i| self.plugin.parameter_info(i))
            .map(|info| ParamId::new(info.id))
            .collect()
    }

    /// The descriptor of parameter `id`; `None` if the plugin has no such
    /// parameter or no controller. Walks the list, so prefer
    /// [`parameter_list`](Self::parameter_list) for more than one. Each call is
    /// an O(n) walk of FFI calls into the plugin.
    pub fn parameter_info(&self, id: ParamId) -> Option<Vst3ParameterInfo> {
        (0..self.parameter_count())
            .filter_map(|i| self.plugin.parameter_info(i))
            .find(|info| info.id == id.get())
    }

    /// The descriptor at list position `index`, for the walks that enumerate.
    pub(in crate::host) fn parameter_info_at(&self, index: u32) -> Option<Vst3ParameterInfo> {
        self.plugin.parameter_info(index)
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
    pub fn parameter_list(&self) -> Vec<ParameterInfo> {
        let units = self.units();
        let groups = unit_names(&units);

        (0..self.parameter_count())
            .filter_map(|i| {
                let info = self.parameter_info_at(i)?;
                let plain = self.parameter_plain_range(ParamId::new(info.id));
                Some(build_param_info(info, plain, &groups))
            })
            .collect()
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
    pub fn parameter_plain_range(&self, id: ParamId) -> Option<PlainRange> {
        let at = |n: f64| self.plugin.normalized_to_plain(id.get(), n);

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
    pub fn note_expression_count(&self, bus_index: i32, channel: i16) -> u32 {
        self.plugin.note_expression_count(bus_index, channel)
    }

    /// The `IProcessContextRequirements::getProcessContextRequirements`
    /// bitmask the plugin returned at load (see [`process_context_flags`]), or
    /// `None` if it doesn't implement the interface, which a host treats as
    /// "wants everything".
    ///
    /// [`process_context_flags`]: crate::types::process_context_flags
    pub fn context_requirements(&self) -> Option<u32> {
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
        bus_index: i32,
        channel: i16,
        index: u32,
    ) -> Option<Vst3NoteExpressionInfo> {
        self.plugin.note_expression_info(bus_index, channel, index)
    }

    /// Number of key-switch (articulation) entries the plugin exposes on the
    /// given event `bus_index` / MIDI `channel`. Returns `0` if the plugin
    /// doesn't implement `IKeyswitchController`.
    ///
    /// Pair with [`keyswitch_info`](Self::keyswitch_info) to enumerate the
    /// articulation map a sample-library instrument advertises.
    pub fn keyswitch_count(&self, bus_index: i32, channel: i16) -> u32 {
        self.plugin.keyswitch_count(bus_index, channel)
    }

    /// Descriptor for the key switch at `index` on the given event `bus_index` /
    /// MIDI `channel` (articulation title, trigger key range, kind). Returns
    /// `None` if the index is out of range or the plugin doesn't implement
    /// `IKeyswitchController`.
    pub fn keyswitch_info(
        &self,
        bus_index: i32,
        channel: i16,
        index: u32,
    ) -> Option<Vst3KeyswitchInfo> {
        self.plugin.keyswitch_info(bus_index, channel, index)
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
    ///
    /// # Panics
    ///
    /// In debug builds, panics if called off the thread registered with
    /// [`tutti_plugin_types::mark_main_thread`].
    pub fn remap_param_id(
        &self,
        plugin_to_replace_uid: &[i8; 16],
        old_param_id: ParamId,
    ) -> Option<ParamId> {
        tutti_plugin_types::assert_main_thread();
        self.plugin
            .compatible_param_id(plugin_to_replace_uid, old_param_id.get())
            .map(ParamId::new)
    }

    /// Resolves a well-known parameter *function name* (the VST3 spec defines
    /// roles like "Wet/Dry Mix", "Master Volume", "Resonance") to the plugin's
    /// `ParamID` for that role, scoped to `unit_id`. Returns `None` if the role
    /// is unknown to the plugin or it doesn't implement `IParameterFunctionName`.
    ///
    /// Lets a host bind a generic "mix" knob to whatever parameter the plugin
    /// uses for it, without hard-coding parameter indices. Main/UI thread.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if called off the thread registered with
    /// [`tutti_plugin_types::mark_main_thread`].
    pub fn param_id_for_function_name(&self, unit_id: i32, function_name: &str) -> Option<ParamId> {
        tutti_plugin_types::assert_main_thread();
        self.plugin
            .param_id_for_function_name(unit_id, function_name)
            .map(ParamId::new)
    }

    /// Maps the plugin's physical UI controls to the note-expression dimensions
    /// they drive, on the given event `bus_index` / MIDI `channel`. Returns one
    /// `(physical_ui_type, note_expression_type)` pair per physical control
    /// (X/Y movement, pressure — see [`physical_ui_type`](crate::physical_ui_type)),
    /// or an empty vec if the plugin doesn't implement
    /// `INoteExpressionPhysicalUIMapping`.
    ///
    /// The host allocates the list; the plugin fills the note-expression id each
    /// physical control is wired to. Main/UI thread.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if called off the thread registered with
    /// [`tutti_plugin_types::mark_main_thread`].
    pub fn physical_ui_mapping(&self, bus_index: i32, channel: i16) -> Vec<(u32, u32)> {
        tutti_plugin_types::assert_main_thread();
        self.plugin.physical_ui_mapping(bus_index, channel)
    }

    /// Export the plugin's parameter remote-control layout as XML, for the
    /// representation identified by `(vendor, name, version, host)`. Returns the
    /// XML string, or `None` if the plugin doesn't implement
    /// `IXmlRepresentationController` or produced nothing.
    ///
    /// Hardware controller surfaces use this to lay out a plugin's parameters.
    /// Main/UI thread.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if called off the thread registered with
    /// [`tutti_plugin_types::mark_main_thread`].
    pub fn xml_representation(
        &self,
        vendor: &str,
        name: &str,
        version: &str,
        host: &str,
    ) -> Option<String> {
        tutti_plugin_types::assert_main_thread();
        self.plugin.xml_representation(vendor, name, version, host)
    }

    /// Tells the plugin the host's current automation read/write mode via
    /// `IAutomationState`.
    ///
    /// Some plugins change behaviour during automation playback vs recording
    /// (e.g. snapping a knob to the automation lane while reading). No-op if the
    /// plugin doesn't implement `IAutomationState`. Returns `true` if the call
    /// was delivered. Must run on the main/UI thread.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if called off the thread registered with
    /// [`tutti_plugin_types::mark_main_thread`].
    pub fn apply_automation_state(&mut self, mode: AutomationMode) -> bool {
        tutti_plugin_types::assert_main_thread();
        self.plugin
            .set_automation_state(tutti_vst3_ffi::automation_state::from_mode(mode))
    }
}

/// Whether `req` asks for any transport field; no interface wants everything.
fn wants_transport(req: Option<u32>) -> bool {
    use crate::types::process_context_flags as f;
    const TRANSPORT_BITS: u32 = f::NEED_TEMPO
        | f::NEED_PROJECT_TIME_MUSIC
        | f::NEED_BAR_POSITION_MUSIC
        | f::NEED_CYCLE_MUSIC
        | f::NEED_TIME_SIGNATURE
        | f::NEED_TRANSPORT_STATE
        | f::NEED_CONTINOUS_TIME_SAMPLES;
    req.is_none_or(|req| req & TRANSPORT_BITS != 0)
}

/// Whether `req` asks for the chord field; no interface wants everything.
fn wants_sequencer_context(req: Option<u32>) -> bool {
    use crate::types::process_context_flags as f;
    req.is_none_or(|req| req & f::NEED_CHORD != 0)
}

/// `None` for the sentinel the FFI layer keeps for "interface not
/// implemented".
pub(crate) fn requirements(raw: u32) -> Option<u32> {
    (raw != u32::MAX).then_some(raw)
}

/// Index a plugin's unit list by id, keeping only units that can name a group.
///
/// Two exclusions, both deliberate:
///
/// - **The root unit** (`unit_ids::ROOT`, id 0) is every plugin's implicit
///   top-level unit, and its name is the plugin's own. Every parameter that
///   declares no unit reports 0, so mapping it would label the entire flat
///   majority with the plugin name — noise on exactly the parameters that have
///   no group.
/// - **Unnamed units.** A unit with an empty name resolves to no group rather
///   than to an empty label, which is the same answer by a shorter route.
pub(super) fn unit_names(units: &[Vst3UnitInfo]) -> HashMap<i32, &str> {
    units
        .iter()
        .filter(|u| u.id != unit_ids::ROOT && !u.name.is_empty())
        .map(|u| (u.id, u.name.as_str()))
        .collect()
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
    groups: &HashMap<i32, &str>,
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
                default: r.to_plain(info.default_normalized_value),
            }
        }
        None => ParamRange::Normalized {
            default: info.default_normalized_value,
        },
    };

    // The SDK spells out the encoding at `ivsteditcontroller.h:53`:
    // 0 continuous, 1 toggle, otherwise `max - min` so the position count is
    // one more than the step count.
    let steps = match info.step_count {
        0 => ParamSteps::Continuous,
        1 => ParamSteps::Toggle,
        n if n > 1 => ParamSteps::Enumerated(n as u32 + 1),
        // Negative is out of contract; report it as unsaid rather than guessing.
        _ => ParamSteps::Unknown,
    };

    ParameterInfo {
        // VST3 `ParamID` — opaque and plugin-chosen, not a list position; see
        // the ParamID-vs-index note above.
        id: info.id.into(),
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
            .get(&info.unit_id)
            .map(|n| n.to_string())
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod unit_group_tests {
    use super::*;

    /// Builds a unit as the plugin would report it, with no program list.
    fn unit(id: i32, name: &str) -> Vst3UnitInfo {
        Vst3UnitInfo {
            id,
            parent: Some(unit_ids::ROOT),
            name: name.to_string(),
            program_list: None,
            program_list_id_raw: unit_ids::NO_PROGRAM_LIST,
        }
    }

    /// A parameter naming a published unit is labelled with that unit's name.
    #[test]
    fn a_parameter_takes_the_name_of_the_unit_it_declares() {
        let units = [unit(1, "Filter"), unit(2, "Amp")];
        let groups = unit_names(&units);
        assert_eq!(groups.get(&1).copied(), Some("Filter"));
        assert_eq!(groups.get(&2).copied(), Some("Amp"));
    }

    /// The root unit names no group.
    ///
    /// This is the case that decides whether grouping is useful or noise:
    /// every parameter that declares no unit reports id 0, so if the root were
    /// mapped, the flat majority of parameters on every plugin would be
    /// labelled with the plugin's own name.
    #[test]
    fn the_root_unit_is_not_a_group() {
        let units = [unit(unit_ids::ROOT, "TAL-NoiseMaker"), unit(1, "Filter")];
        let groups = unit_names(&units);
        assert_eq!(
            groups.get(&unit_ids::ROOT),
            None,
            "the root unit's name is the plugin's, not a group's"
        );
        assert_eq!(groups.get(&1).copied(), Some("Filter"));
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
        assert_eq!(groups.get(&7), None);
    }

    /// A unit the plugin published without a name yields no group, rather than
    /// an empty heading a UI would render as a blank section.
    #[test]
    fn an_unnamed_unit_is_not_a_group() {
        let units = [unit(1, "")];
        let groups = unit_names(&units);
        assert_eq!(groups.get(&1), None);
    }
}

#[cfg(test)]
mod requirements_tests {
    use super::{requirements, wants_sequencer_context, wants_transport};
    use crate::types::process_context_flags as f;

    /// The FFI layer keeps `u32::MAX` for "interface not implemented"; every
    /// other word, including `0` (implemented, asks for nothing), is a real
    /// answer.
    ///
    /// Mutation (run): `!=` to `==` in `requirements` fails all three rows;
    /// returning `Some(raw)` always fails the first.
    #[test]
    fn only_the_sentinel_means_no_interface() {
        assert_eq!(requirements(u32::MAX), None);
        assert_eq!(requirements(0), Some(0));
        assert_eq!(requirements(f::NEED_TEMPO), Some(f::NEED_TEMPO));
    }

    /// A plugin without the interface wants every field; one that has it wants
    /// exactly what it flags.
    ///
    /// Mutation (run): replacing `is_none_or` with `is_some_and` in the transport
    /// predicate fails the `None` row; dropping `NEED_CHORD` from the
    /// sequencer predicate (testing `req != 0`) fails the tempo-only row.
    #[test]
    fn no_interface_wants_everything() {
        assert!(wants_transport(None));
        assert!(wants_sequencer_context(None));
        assert!(!wants_transport(Some(0)));
        assert!(!wants_sequencer_context(Some(0)));
        assert!(wants_transport(Some(f::NEED_TEMPO)));
        assert!(!wants_sequencer_context(Some(f::NEED_TEMPO)));
        assert!(wants_sequencer_context(Some(f::NEED_CHORD)));
    }
}
