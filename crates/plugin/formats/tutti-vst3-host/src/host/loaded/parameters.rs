//! The parameter tree and the controller's optional interfaces: note expression, keyswitches, remaps, representations.

use super::*;

impl Vst3Loaded {
    // ── Parameters: ParamID vs index ──────────────────────────────────────────
    //
    // `IEditController` uses TWO different address spaces and conflating them
    // is silent — you get a plausible `f64` for the wrong parameter:
    //
    // - `getParameterInfo(int32 paramIndex, ..)` is addressed by **index**,
    //   `0 .. getParameterCount()`, and yields the `ParameterInfo` whose `.id`
    //   is the ParamID.
    // - `getParamNormalized(ParamID)` / `setParamNormalized(ParamID, ..)` are
    //   addressed by **ParamID**, an opaque plugin-chosen `uint32`. It is not
    //   an index and is under no obligation to be small, dense, or ordered —
    //   plenty of plugins derive it from a hash of the parameter name.
    //
    // The methods below are therefore named and documented for the space they
    // actually take, and the `*_by_index` pair resolves `index → ParamID`
    // through `getParameterInfo` instead of passing an index straight into a
    // ParamID slot (which reads whichever parameter happens to own that numeric
    // id, or nothing at all).

    /// Number of automatable parameters exposed by the edit controller — the
    /// exclusive upper bound of the **index** space.
    /// Returns `0` if the plugin has no controller.
    pub fn parameter_count(&self) -> u32 {
        self.plugin.parameter_count()
    }

    /// The **ParamID** of the parameter at `index` in `0 .. parameter_count()`.
    /// `None` when the index is out of range or the plugin has no controller.
    ///
    /// This is the bridge between the two address spaces: anything iterating
    /// `0..parameter_count()` must go through it (or
    /// [`parameter_info`](Self::parameter_info)) before calling
    /// [`Control::parameter`] / [`Control::set_parameter`].
    pub fn parameter_id_at(&self, index: u32) -> Option<u32> {
        self.parameter_info(index).map(|info| info.id)
    }

    /// Reads the normalized value of the parameter at **index**, resolving the
    /// index to its ParamID first. `None` if the index is out of range or the
    /// plugin has no controller.
    pub fn parameter_by_index(&self, index: u32) -> Option<f64> {
        let id = ParamId::new(self.parameter_id_at(index)?);
        Control::parameter(self, id).map(Normalized::get)
    }

    /// Writes a normalized `value` to the parameter at **index**, resolving the
    /// index to its ParamID first. Returns `false` when the index is out of
    /// range or the plugin has no controller (nothing was written).
    pub fn set_parameter_by_index(&mut self, index: u32, value: f64) -> bool {
        match self.parameter_id_at(index) {
            Some(id) => {
                Control::set_parameter(self, ParamId::new(id), Normalized::new(value));
                true
            }
            None => false,
        }
    }

    /// Descriptor for the parameter at **index** (title, units, flags, default,
    /// and `id` — the ParamID to address it by).
    /// Returns `None` if the index is out of range or the plugin has no
    /// controller.
    pub fn parameter_info(&self, index: u32) -> Option<Vst3ParameterInfo> {
        self.plugin.parameter_info(index)
    }

    /// Every parameter the plugin advertises, as the SHARED
    /// [`tutti_plugin_types::ParameterInfo`] — the boundary vocabulary every
    /// host crate speaks.
    ///
    /// This is the single VST3 `narrow -> shared` mapping. VST3 addresses
    /// parameters by an opaque `ParamID` but *enumerates* them by index, so
    /// this walks `0 .. parameter_count()` and carries each descriptor's own
    /// `id` into the entry's [`ParamId`] — index `n` is not id `n`, and a
    /// caller must address through the entry rather than its position.
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
                let info = self.parameter_info(i)?;
                let plain = self.parameter_plain_range(info.id);
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
    pub fn parameter_plain_range(&self, id: u32) -> Option<(f64, f64)> {
        let at = |n: f64| self.plugin.normalized_to_plain(id, n);

        let (lo, mid, hi) = (at(0.0)?, at(0.5)?, at(1.0)?);
        if !lo.is_finite() || !mid.is_finite() || !hi.is_finite() {
            return None;
        }
        // A range the endpoints alone would accept but whose interior
        // contradicts them is not a mappable range. Inclusive because a
        // legitimately constant parameter probes flat.
        let monotonic = (lo <= mid && mid <= hi) || (hi <= mid && mid <= lo);
        monotonic.then_some(if lo <= hi { (lo, hi) } else { (hi, lo) })
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

    /// Raw `IProcessContextRequirements::getProcessContextRequirements` bitmask
    /// the plugin returned at load (see [`process_context_flags`]). `u32::MAX`
    /// means the plugin doesn't implement the interface — treat as "wants
    /// everything", matching the pre-interface unconditional behaviour.
    ///
    /// [`process_context_flags`]: crate::types::process_context_flags
    pub fn context_requirements(&self) -> u32 {
        self.plugin.process_context_requirements()
    }

    /// `true` if the plugin asked for any transport field (tempo, playhead,
    /// bar, cycle, time-sig, or transport state). A plugin that implements
    /// `IProcessContextRequirements` and requests none of these does not want a
    /// transport snapshot each block; one that doesn't implement the interface
    /// (`u32::MAX`) wants everything.
    pub fn wants_transport(&self) -> bool {
        use crate::types::process_context_flags as f;
        let req = self.plugin.process_context_requirements();
        const TRANSPORT_BITS: u32 = f::NEED_TEMPO
            | f::NEED_PROJECT_TIME_MUSIC
            | f::NEED_BAR_POSITION_MUSIC
            | f::NEED_CYCLE_MUSIC
            | f::NEED_TIME_SIGNATURE
            | f::NEED_TRANSPORT_STATE
            | f::NEED_CONTINOUS_TIME_SAMPLES;
        req & TRANSPORT_BITS != 0
    }

    /// `true` if the plugin asked for the host-track chord field
    /// (`kNeedChord`) — the VST3 signal that it consumes sequencer context
    /// (chord / scale). Distinct from note-expression.
    pub fn wants_sequencer_context(&self) -> bool {
        use crate::types::process_context_flags as f;
        self.plugin.process_context_requirements() & f::NEED_CHORD != 0
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
        old_param_id: u32,
    ) -> Option<u32> {
        tutti_plugin_types::assert_main_thread();
        self.plugin
            .compatible_param_id(plugin_to_replace_uid, old_param_id)
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
    pub fn param_id_for_function_name(&self, unit_id: i32, function_name: &str) -> Option<u32> {
        tutti_plugin_types::assert_main_thread();
        self.plugin
            .param_id_for_function_name(unit_id, function_name)
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
    /// `IAutomationState`. `state` is one of the
    /// [`automation_state`](crate::automation_state) constants
    /// (`NONE` / `READ` / `WRITE` / `READ_WRITE`).
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
    pub fn apply_automation_state(&mut self, state: i32) -> bool {
        tutti_plugin_types::assert_main_thread();
        self.plugin.set_automation_state(state)
    }
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
    plain: Option<(f64, f64)>,
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
        Some((min, max)) => {
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
