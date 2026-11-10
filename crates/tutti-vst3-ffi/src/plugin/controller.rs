//! `IEditController` and its optional interfaces: parameters, note expression, keyswitches, remaps, units, representations and MIDI.

use super::*;

impl Plugin {
    // ── Parameters ───────────────────────────────────────────────────────────

    /// How many parameters the controller reports; zero without a controller.
    #[must_use]
    pub fn parameter_count(&self, main: &MainThread) -> u32 {
        let _scope = self.scoped(main);
        match self.controller.get() {
            // SAFETY: a plain getter on a live controller.
            // A negative count is a broken plugin's, not four billion parameters.
            Some(c) => unsafe { c.getParameterCount().max(0).cast_unsigned() },
            None => 0,
        }
    }

    /// The `index`-th parameter's description.
    #[must_use]
    pub fn parameter_info(&self, main: &MainThread, index: u32) -> Option<Vst3ParameterInfo> {
        let _scope = self.scoped(main);
        let controller = self.controller.get()?;
        let mut raw = zeroed::<vst3::Steinberg::Vst::ParameterInfo>();
        // SAFETY: `raw` is a valid out-parameter.
        check(unsafe { controller.getParameterInfo(i32::try_from(index).ok()?, &mut raw) }).ok()?;
        Some(Vst3ParameterInfo::from_c(&raw))
    }

    /// `normalizedParamToPlain` for the parameter `id`; `None` without a
    /// controller.
    #[must_use]
    pub fn normalized_to_plain(&self, main: &MainThread, id: u32, normalized: f64) -> Option<f64> {
        let _scope = self.scoped(main);
        let controller = self.controller.get()?;
        // SAFETY: a plain conversion on a live controller.
        Some(unsafe { controller.normalizedParamToPlain(id, normalized) })
    }

    /// The parameter's current normalized value; `None` without a controller.
    #[must_use]
    pub fn parameter_normalized(&self, main: &MainThread, id: ParamId) -> Option<f64> {
        let _scope = self.scoped(main);
        let controller = self.controller.get()?;
        // SAFETY: a plain getter on a live controller.
        Some(unsafe { controller.getParamNormalized(id.get()) })
    }

    /// Sets the parameter's normalized value; `false` without a controller.
    pub fn set_parameter_normalized(&self, main: &MainThread, id: ParamId, value: f64) -> bool {
        let _scope = self.scoped(main);
        let Some(controller) = self.controller.get() else {
            return false;
        };
        // SAFETY: a plain setter on a live controller.
        unsafe { controller.setParamNormalized(id.get(), value) };
        true
    }

    /// The plugin's text for a normalized value.
    #[must_use]
    pub fn parameter_text(
        &self,
        main: &MainThread,
        id: ParamId,
        normalized: f64,
    ) -> Option<String> {
        let _scope = self.scoped(main);
        let controller = self.controller.get()?;
        let mut string: String128 = [0; 128];
        // SAFETY: `string` is a valid out-parameter of the size the ABI names.
        check(unsafe { controller.getParamStringByValue(id.get(), normalized, &mut string) })
            .ok()?;
        Some(utf16_to_string(&string))
    }

    /// The normalized value for the plugin's reading of `text`, if it parses
    /// and the answer is finite.
    ///
    /// `text` is cut to 127 UTF-16 units, the `String128` the ABI allows.
    #[must_use]
    pub fn parameter_from_text(&self, main: &MainThread, id: ParamId, text: &str) -> Option<f64> {
        let _scope = self.scoped(main);
        let controller = self.controller.get()?;
        let mut buf: Vec<u16> = text.encode_utf16().take(127).collect();
        buf.push(0);
        let mut value: f64 = 0.0;
        // SAFETY: `buf` is a NUL-terminated UTF-16 string that outlives the
        // call, and `value` a valid out-parameter. The ABI takes it mutable but
        // does not write through it.
        check(unsafe { controller.getParamValueByString(id.get(), buf.as_mut_ptr(), &mut value) })
            .ok()?;
        value.is_finite().then_some(value)
    }

    // ── Notes, keyswitches, remaps, units, representations ───────────────────

    /// `INoteExpressionController::getNoteExpressionCount`, floored at zero;
    /// zero without the interface.
    #[must_use]
    pub fn note_expression_count(&self, main: &MainThread, bus_index: i32, channel: i16) -> u32 {
        let _scope = self.scoped(main);
        match &self.ext.note_expression {
            // SAFETY: a plain getter on a live interface.
            Some(c) => unsafe {
                c.getNoteExpressionCount(bus_index, channel)
                    .max(0)
                    .cast_unsigned()
            },
            None => 0,
        }
    }

    /// The `index`-th note-expression type.
    #[must_use]
    pub fn note_expression_info(
        &self,
        main: &MainThread,
        bus_index: i32,
        channel: i16,
        index: u32,
    ) -> Option<Vst3NoteExpressionInfo> {
        let _scope = self.scoped(main);
        let controller = self.ext.note_expression.as_ref()?;
        let mut raw = zeroed::<vst3::Steinberg::Vst::NoteExpressionTypeInfo>();
        // SAFETY: `raw` is a valid out-parameter.
        check(unsafe {
            controller.getNoteExpressionInfo(
                bus_index,
                channel,
                i32::try_from(index).ok()?,
                &mut raw,
            )
        })
        .ok()?;
        Some(Vst3NoteExpressionInfo::from_c(&raw))
    }

    /// `IKeyswitchController::getKeyswitchCount`, floored at zero; zero without
    /// the interface.
    #[must_use]
    pub fn keyswitch_count(&self, main: &MainThread, bus_index: i32, channel: i16) -> u32 {
        let _scope = self.scoped(main);
        match &self.ext.keyswitch {
            // SAFETY: a plain getter on a live interface.
            Some(c) => unsafe {
                c.getKeyswitchCount(bus_index, channel)
                    .max(0)
                    .cast_unsigned()
            },
            None => 0,
        }
    }

    /// The `index`-th keyswitch.
    #[must_use]
    pub fn keyswitch_info(
        &self,
        main: &MainThread,
        bus_index: i32,
        channel: i16,
        index: u32,
    ) -> Option<Vst3KeyswitchInfo> {
        let _scope = self.scoped(main);
        let controller = self.ext.keyswitch.as_ref()?;
        let mut raw = zeroed::<vst3::Steinberg::Vst::KeyswitchInfo>();
        // SAFETY: `raw` is a valid out-parameter.
        check(unsafe {
            controller.getKeyswitchInfo(bus_index, channel, i32::try_from(index).ok()?, &mut raw)
        })
        .ok()?;
        Some(Vst3KeyswitchInfo::from_c(&raw))
    }

    /// `IRemapParamID::getCompatibleParamID`: the id the plugin now gives to a
    /// parameter a plugin identified by `plugin_to_replace_uid` once had as
    /// `old_param_id`.
    #[must_use]
    pub fn compatible_param_id(
        &self,
        main: &MainThread,
        plugin_to_replace_uid: &[i8; 16],
        old_param_id: u32,
    ) -> Option<u32> {
        let _scope = self.scoped(main);
        let remap = self.ext.remap_param_id.as_ref()?;
        let mut new_param_id: u32 = 0;
        // SAFETY: the uid points at 16 bytes for the call and the id is a valid
        // out-parameter.
        let result = unsafe {
            remap.getCompatibleParamID(
                plugin_to_replace_uid as *const TUID,
                old_param_id,
                &mut new_param_id,
            )
        };
        (result == vst3::Steinberg::kResultTrue).then_some(new_param_id)
    }

    /// `IParameterFunctionName::getParameterIDFromFunctionName`.
    #[must_use]
    pub fn param_id_for_function_name(
        &self,
        main: &MainThread,
        unit_id: i32,
        function_name: &str,
    ) -> Option<u32> {
        let _scope = self.scoped(main);
        let ctrl = self.ext.parameter_function_name.as_ref()?;
        let c_name = std::ffi::CString::new(function_name).ok()?;
        let mut param_id: u32 = 0;
        // SAFETY: `c_name` is a NUL-terminated string that outlives the call and
        // `param_id` a valid out-parameter.
        check(unsafe {
            ctrl.getParameterIDFromFunctionName(unit_id, c_name.as_ptr(), &mut param_id)
        })
        .ok()?;
        Some(param_id)
    }

    /// Every program list the plugin declares.
    #[must_use]
    pub fn program_lists(&self, main: &MainThread) -> Vec<Vst3ProgramListInfo> {
        let _scope = self.scoped(main);
        let Some(ctrl) = self.ext.unit_info.as_ref() else {
            return Vec::new();
        };
        // SAFETY: a plain getter on a live interface.
        let count = unsafe { ctrl.getProgramListCount() }.max(0);
        let mut lists = Vec::with_capacity(count as usize);
        for i in 0..count {
            let mut raw = zeroed::<vst3::Steinberg::Vst::ProgramListInfo>();
            // SAFETY: `raw` is a valid out-parameter.
            if is_ok(unsafe { ctrl.getProgramListInfo(i, &mut raw) }) {
                lists.push(Vst3ProgramListInfo::from_c(&raw));
            }
        }
        lists
    }

    /// Every unit the plugin declares, with each unit's program list resolved
    /// against the lists the plugin actually has.
    #[must_use]
    pub fn units(&self, main: &MainThread) -> Vec<Vst3UnitInfo> {
        let _scope = self.scoped(main);
        let Some(ctrl) = self.ext.unit_info.as_ref() else {
            return Vec::new();
        };
        let known: Vec<i32> = self.program_lists(main).into_iter().map(|l| l.id).collect();
        // SAFETY: a plain getter on a live interface.
        let count = unsafe { ctrl.getUnitCount() }.max(0);
        let mut units = Vec::with_capacity(count as usize);
        for i in 0..count {
            let mut raw = zeroed::<vst3::Steinberg::Vst::UnitInfo>();
            // SAFETY: `raw` is a valid out-parameter.
            if is_ok(unsafe { ctrl.getUnitInfo(i, &mut raw) }) {
                let mut unit = Vst3UnitInfo::from_c(&raw);
                unit.resolve_program_list(&known);
                units.push(unit);
            }
        }
        units
    }

    /// The name of a program in a list.
    #[must_use]
    pub fn program_name(
        &self,
        main: &MainThread,
        list_id: i32,
        program_index: u32,
    ) -> Option<String> {
        let _scope = self.scoped(main);
        let ctrl = self.ext.unit_info.as_ref()?;
        let mut name: String128 = [0; 128];
        // SAFETY: `name` is a valid out-parameter of the size the ABI names.
        check(unsafe {
            ctrl.getProgramName(list_id, i32::try_from(program_index).ok()?, &mut name)
        })
        .ok()?;
        Some(utf16_to_string(&name))
    }

    /// The selected unit.
    #[must_use]
    pub fn selected_unit(&self, main: &MainThread) -> Option<i32> {
        let _scope = self.scoped(main);
        let ctrl = self.ext.unit_info.as_ref()?;
        // SAFETY: a plain getter on a live interface.
        Some(unsafe { ctrl.getSelectedUnit() })
    }

    /// Selects a unit; `false` without the interface or if refused.
    #[must_use]
    pub fn select_unit(&self, main: &MainThread, unit_id: i32) -> bool {
        let _scope = self.scoped(main);
        let Some(ctrl) = self.ext.unit_info.as_ref() else {
            return false;
        };
        // SAFETY: a plain call on a live interface.
        is_ok(unsafe { ctrl.selectUnit(unit_id) })
    }

    /// The unit a bus channel belongs to. The ints are the ABI's, passed as
    /// given.
    #[must_use]
    pub fn unit_by_bus(
        &self,
        main: &MainThread,
        media_type: i32,
        direction: i32,
        bus_index: i32,
        channel: i32,
    ) -> Option<i32> {
        let _scope = self.scoped(main);
        let ctrl = self.ext.unit_info.as_ref()?;
        let mut unit_id: i32 = 0;
        // SAFETY: `unit_id` is a valid out-parameter.
        check(unsafe {
            ctrl.getUnitByBus(media_type, direction, bus_index, channel, &mut unit_id)
        })
        .ok()?;
        Some(unit_id)
    }

    /// `(physical UI type, note-expression type)` for the three physical
    /// controls the SDK defines, as the plugin maps them; `u32::MAX` for an
    /// unmapped one. Empty without the interface or if refused.
    #[must_use]
    pub fn physical_ui_mapping(
        &self,
        main: &MainThread,
        bus_index: i32,
        channel: i16,
    ) -> Vec<(u32, u32)> {
        let _scope = self.scoped(main);
        let Some(ctrl) = self.ext.physical_ui_mapping.as_ref() else {
            return Vec::new();
        };
        let mut entries: Vec<PhysicalUIMap> = (0..3)
            .map(|i| PhysicalUIMap {
                physicalUITypeID: i,
                noteExpressionTypeID: u32::MAX,
            })
            .collect();
        let mut list = PhysicalUIMapList {
            count: entries.len() as u32,
            map: entries.as_mut_ptr(),
        };
        // SAFETY: `list.map` points at `list.count` entries that outlive the
        // call.
        if !is_ok(unsafe { ctrl.getPhysicalUIMapping(bus_index, channel, &mut list) }) {
            return Vec::new();
        }
        entries
            .iter()
            .map(|e| (e.physicalUITypeID, e.noteExpressionTypeID))
            .collect()
    }

    /// The plugin's XML parameter representation, if it offers one.
    ///
    /// Each identifying string is cut to 63 bytes, the `char8[64]` the ABI
    /// allows.
    #[must_use]
    pub fn xml_representation(
        &self,
        main: &MainThread,
        vendor: &str,
        name: &str,
        version: &str,
        host: &str,
    ) -> Option<String> {
        let _scope = self.scoped(main);
        let ctrl = self.ext.xml_representation.as_ref()?;
        let mut info = zeroed::<RepresentationInfo>();
        fill_char8_64(&mut info.vendor, vendor);
        fill_char8_64(&mut info.name, name);
        fill_char8_64(&mut info.version, version);
        fill_char8_64(&mut info.host, host);

        let stream = BStream::new();
        let stream_ptr = stream.as_com_ref::<IBStream>()?;
        // SAFETY: `info` and the stream are valid for the call; `stream` keeps
        // the stream alive.
        check(unsafe { ctrl.getXmlRepresentationStream(&mut info, stream_ptr.as_ptr()) }).ok()?;
        let bytes = stream.data();
        (!bytes.is_empty()).then(|| String::from_utf8_lossy(&bytes).into_owned())
    }

    /// `IAutomationState::setAutomationState`; `false` without the interface.
    pub fn set_automation_state(&self, main: &MainThread, state: i32) -> bool {
        let _scope = self.scoped(main);
        match &self.ext.automation_state {
            Some(a) => {
                // SAFETY: a plain call on a live interface.
                unsafe { a.setAutomationState(state) };
                true
            }
            None => false,
        }
    }

    // ── MIDI ─────────────────────────────────────────────────────────────────

    /// Whether the controller implements `IMidiMapping`.
    #[must_use]
    pub fn has_midi_mapping(&self, main: &MainThread) -> bool {
        let _scope = self.scoped(main);
        self.controller
            .get()
            .is_some_and(|c| c.cast::<IMidiMapping>().is_some())
    }

    /// `IMidiMapping::getMidiControllerAssignment` for bus 0: the parameter
    /// the plugin maps a CC to, if it maps one (`kResultTrue`).
    #[must_use]
    pub fn midi_controller_assignment(
        &self,
        main: &MainThread,
        channel: i16,
        controller: i16,
    ) -> Option<u32> {
        let _scope = self.scoped(main);
        let mapping = self.controller.get()?.cast::<IMidiMapping>()?;
        self.midi_assignment_with(&mapping, channel, controller)
    }

    pub(super) fn midi_assignment_with(
        &self,
        mapping: &ComPtr<IMidiMapping>,
        channel: i16,
        controller: i16,
    ) -> Option<u32> {
        let mut id: u32 = 0;
        // SAFETY: `id` is a valid out-parameter.
        let rc = unsafe { mapping.getMidiControllerAssignment(0, channel, controller, &mut id) };
        (rc == vst3::Steinberg::kResultTrue).then_some(id)
    }

    /// Every `(channel, controller)` the plugin maps to a parameter, for the
    /// channels and controller numbers named: one `queryInterface` for the
    /// whole sweep.
    pub fn midi_controller_assignments(
        &self,
        main: &MainThread,
        channels: std::ops::Range<i16>,
        controllers: std::ops::Range<i16>,
        mut visit: impl FnMut(i16, i16, u32),
    ) {
        let _scope = self.scoped(main);
        let Some(mapping) = self.controller.get().and_then(|c| c.cast::<IMidiMapping>()) else {
            return;
        };
        for channel in channels {
            for controller in controllers.clone() {
                if let Some(id) = self.midi_assignment_with(&mapping, channel, controller) {
                    visit(channel, controller, id);
                }
            }
        }
    }

    /// The plugin's `IMidiLearn`, if the controller implements it: a handle the
    /// host keeps beside the plugin and forwards captured CCs through.
    #[must_use]
    pub fn midi_learn(&self) -> Option<MidiLearn> {
        self.ext.midi_learn.clone().map(|learn| MidiLearn {
            learn,
            shared: self.shared.clone(),
            _module: self.module.clone(),
        })
    }
}

/// Writes `src` into a `char8[64]`, cut to 63 bytes and NUL-terminated.
pub(super) fn fill_char8_64(dst: &mut [i8; 64], src: &str) {
    let bytes = src.as_bytes();
    let n = bytes.len().min(dst.len() - 1);
    for (slot, &b) in dst.iter_mut().zip(&bytes[..n]) {
        *slot = b as i8;
    }
    dst[n] = 0;
}

#[cfg(test)]
mod char8_fill_tests {
    use super::fill_char8_64;

    /// A short string is copied verbatim and NUL-terminated.
    #[test]
    fn fills_and_terminates() {
        let mut buf = [0i8; 64];
        fill_char8_64(&mut buf, "Acme");
        assert_eq!(&buf[..4], &[b'A' as i8, b'c' as i8, b'm' as i8, b'e' as i8]);
        assert_eq!(buf[4], 0);
    }

    /// An over-long string is truncated, always leaving room for the NUL
    /// terminator at index 63.
    #[test]
    fn truncates_leaving_room_for_terminator() {
        let mut buf = [1i8; 64];
        let long = "x".repeat(100);
        fill_char8_64(&mut buf, &long);
        // 63 chars written, last slot is the terminator.
        assert!(buf[..63].iter().all(|&b| b == b'x' as i8));
        assert_eq!(buf[63], 0);
    }

    /// An empty string yields an immediate terminator.
    #[test]
    fn empty_is_just_terminator() {
        let mut buf = [9i8; 64];
        fill_char8_64(&mut buf, "");
        assert_eq!(buf[0], 0);
    }
}
