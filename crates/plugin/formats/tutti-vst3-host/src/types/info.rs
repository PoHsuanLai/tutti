//! Newtype wrappers over the vst3 crate's `BusInfo`, `ParameterInfo`, and
//! `NoteExpressionTypeInfo`.

use vst3::Steinberg::Vst::IAutomationState_::AutomationStates_;
use vst3::Steinberg::Vst::KeyswitchTypeIDs_;
use vst3::Steinberg::Vst::NoteExpressionTypeInfo_::NoteExpressionTypeFlags_;
use vst3::Steinberg::Vst::ParameterInfo_::ParameterFlags_;
use vst3::Steinberg::Vst::{ePrefetchableSupport_, PhysicalUITypeIDs_};

use crate::helpers::utf16_to_string;

/// Host-facing bus descriptor. Thin wrapper around `vst3::Steinberg::Vst::BusInfo`
/// whose field layout is retained verbatim for FFI, but whose only public
/// accessor for the UTF-16 name is [`BusInfo::name_string`].
#[derive(Clone)]
pub struct BusInfo {
    pub(crate) inner: vst3::Steinberg::Vst::BusInfo,
}

impl Default for BusInfo {
    fn default() -> Self {
        Self {
            inner: unsafe { std::mem::zeroed() },
        }
    }
}

impl BusInfo {
    /// `MediaType` discriminant (`kAudio` or `kEvent`).
    pub fn media_type(&self) -> i32 {
        self.inner.mediaType
    }

    /// `BusDirection` (`kInput` or `kOutput`).
    pub fn direction(&self) -> i32 {
        self.inner.direction
    }

    /// Number of channels on this bus.
    pub fn channel_count(&self) -> i32 {
        self.inner.channelCount
    }

    /// VST3 bus-flags bitfield (`kDefaultActive`, `kIsControlVoltage`, …).
    pub fn flags(&self) -> u32 {
        self.inner.flags
    }

    /// `BusType` (`kMain` or `kAux`).
    pub fn bus_type(&self) -> i32 {
        self.inner.busType
    }

    /// Display name decoded from the UTF-16 buffer.
    pub fn name_string(&self) -> String {
        utf16_to_string(&self.inner.name)
    }

    pub(crate) fn as_mut_inner(&mut self) -> &mut vst3::Steinberg::Vst::BusInfo {
        &mut self.inner
    }
}

/// Host-facing parameter descriptor — a flat snake_case view over the vst3
/// crate's `ParameterInfo`, populated from the C struct returned by
/// `IEditController::getParameterInfo`.
#[derive(Clone)]
pub struct Vst3ParameterInfo {
    /// Stable parameter id used in automation messages.
    pub id: u32,
    /// UTF-16 display title.
    pub title: [u16; 128],
    /// UTF-16 abbreviated title (for narrow UIs).
    pub short_title: [u16; 128],
    /// UTF-16 value units (e.g. "dB", "Hz").
    pub units: [u16; 128],
    /// Number of discrete steps, or 0 for continuous parameters.
    pub step_count: i32,
    /// Default value, normalized to 0.0 – 1.0.
    pub default_normalized_value: f64,
    /// Unit the parameter belongs to (for `IUnitInfo` plugins).
    pub unit_id: i32,
    /// `ParameterFlags_` bitfield. See [`parameter_flags`] for named masks.
    pub flags: i32,
}

impl Default for Vst3ParameterInfo {
    fn default() -> Self {
        Self {
            id: 0,
            title: [0; 128],
            short_title: [0; 128],
            units: [0; 128],
            step_count: 0,
            default_normalized_value: 0.0,
            unit_id: 0,
            flags: 0,
        }
    }
}

impl Vst3ParameterInfo {
    /// Decoded [`title`](Self::title) as a Rust `String`.
    pub fn title_string(&self) -> String {
        utf16_to_string(&self.title)
    }

    /// Decoded [`short_title`](Self::short_title) as a Rust `String`.
    pub fn short_title_string(&self) -> String {
        utf16_to_string(&self.short_title)
    }

    /// Decoded [`units`](Self::units) as a Rust `String`.
    pub fn units_string(&self) -> String {
        utf16_to_string(&self.units)
    }

    /// True if the parameter carries the `kCanAutomate` flag.
    pub fn can_automate(&self) -> bool {
        (self.flags & parameter_flags::CAN_AUTOMATE) != 0
    }

    /// True if the parameter is read-only.
    pub fn is_read_only(&self) -> bool {
        (self.flags & parameter_flags::IS_READ_ONLY) != 0
    }

    /// True if the parameter should be hidden from the host UI.
    pub fn is_hidden(&self) -> bool {
        (self.flags & parameter_flags::IS_HIDDEN) != 0
    }

    /// True if the parameter is the bypass parameter.
    pub fn is_bypass(&self) -> bool {
        (self.flags & parameter_flags::IS_BYPASS) != 0
    }

    /// True if the parameter wraps around at its extremes.
    pub fn is_wrap(&self) -> bool {
        (self.flags & parameter_flags::IS_WRAP) != 0
    }

    pub(crate) fn from_c(c: &vst3::Steinberg::Vst::ParameterInfo) -> Self {
        Self {
            id: c.id,
            title: c.title,
            short_title: c.shortTitle,
            units: c.units,
            step_count: c.stepCount,
            default_normalized_value: c.defaultNormalizedValue,
            unit_id: c.unitId,
            flags: c.flags,
        }
    }
}

/// VST3 `ParameterInfo` flag bits as simple `i32` constants, mirroring
/// `ParameterFlags_` from the Steinberg SDK.
pub mod parameter_flags {
    use super::ParameterFlags_;

    /// Parameter can be automated by the host.
    pub const CAN_AUTOMATE: i32 = ParameterFlags_::kCanAutomate;
    /// Parameter is read-only (display-only, cannot be edited).
    pub const IS_READ_ONLY: i32 = ParameterFlags_::kIsReadOnly;
    /// Parameter wraps around at its extremes (e.g. phase).
    pub const IS_WRAP: i32 = ParameterFlags_::kIsWrapAround;
    /// Parameter is a discrete list (maps to `stepCount` entries).
    pub const IS_LIST: i32 = ParameterFlags_::kIsList;
    /// Parameter should be hidden from host UI.
    pub const IS_HIDDEN: i32 = ParameterFlags_::kIsHidden;
    /// Parameter controls a program change.
    pub const IS_PROGRAM_CHANGE: i32 = ParameterFlags_::kIsProgramChange;
    /// Parameter is the plugin's bypass switch.
    pub const IS_BYPASS: i32 = ParameterFlags_::kIsBypass;
}

/// VST3 `IAutomationState` mode constants, mirroring
/// `IAutomationState_::AutomationStates_` from the Steinberg SDK. The host
/// pushes one of these to the plugin so it can adapt to whether the host is
/// reading, writing, both, or ignoring automation. Pass to
/// [`Vst3Loaded::set_automation_state`](crate::Vst3Loaded::set_automation_state).
pub mod automation_state {
    use super::AutomationStates_;
    use tutti_plugin_types::AutomationMode;

    /// No automation read or write.
    pub const NONE: i32 = AutomationStates_::kNoAutomation;
    /// Host is reading automation.
    pub const READ: i32 = AutomationStates_::kReadState;
    /// Host is writing automation.
    pub const WRITE: i32 = AutomationStates_::kWriteState;
    /// Host is both reading and writing automation.
    pub const READ_WRITE: i32 = AutomationStates_::kReadWriteState;

    /// Encode the format-neutral [`AutomationMode`] as the VST3 `IAutomationState`
    /// bitmask. This mapping lives here — the VST3 crate is the one that knows
    /// both the mode vocabulary and its `IAutomationState` ABI — so
    /// `tutti-plugin-types` stays format-agnostic. Pass the result to
    /// [`Vst3Loaded::set_automation_state`](crate::Vst3Loaded::set_automation_state).
    pub fn from_mode(mode: AutomationMode) -> i32 {
        match mode {
            AutomationMode::Off => NONE,
            AutomationMode::Reading => READ,
            AutomationMode::Writing => WRITE,
            AutomationMode::ReadWriting => READ_WRITE,
        }
    }

    /// Decode a VST3 `IAutomationState` bitmask back into an [`AutomationMode`]
    /// (inverse of [`from_mode`]). Unknown bits beyond read|write are ignored.
    pub fn to_mode(bits: i32) -> AutomationMode {
        match (bits & READ != 0, bits & WRITE != 0) {
            (false, false) => AutomationMode::Off,
            (true, false) => AutomationMode::Reading,
            (false, true) => AutomationMode::Writing,
            (true, true) => AutomationMode::ReadWriting,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn mode_bits_round_trip() {
            for mode in [
                AutomationMode::Off,
                AutomationMode::Reading,
                AutomationMode::Writing,
                AutomationMode::ReadWriting,
            ] {
                assert_eq!(to_mode(from_mode(mode)), mode);
            }
        }

        #[test]
        fn mode_bit_values() {
            // 0=none, 1=read, 2=write, 3=read|write (the SDK values).
            assert_eq!(from_mode(AutomationMode::Off), 0);
            assert_eq!(from_mode(AutomationMode::Reading), 1);
            assert_eq!(from_mode(AutomationMode::Writing), 2);
            assert_eq!(from_mode(AutomationMode::ReadWriting), 3);
        }
    }
}

/// Host-facing note-expression-type descriptor — a flat snake_case view over the
/// vst3 crate's `NoteExpressionTypeInfo`, read from the plugin's
/// `INoteExpressionController::getNoteExpressionInfo`. This is the **read** side
/// of note expression: it tells the host which per-note expression dimensions a
/// plugin supports (the [`crate::NoteExpressionValue`] send side is separate).
#[derive(Clone)]
pub struct Vst3NoteExpressionInfo {
    /// Expression type id (`NoteExpressionTypeID`). Volume/Pan/Tuning/… use the
    /// well-known low ids; plugins may also define custom ones.
    pub type_id: u32,
    /// UTF-16 display title.
    pub title: [u16; 128],
    /// UTF-16 abbreviated title (for narrow UIs).
    pub short_title: [u16; 128],
    /// UTF-16 value units (e.g. "dB", "cent").
    pub units: [u16; 128],
    /// Unit the expression belongs to (for `IUnitInfo` plugins).
    pub unit_id: i32,
    /// Default value, normalized to 0.0 – 1.0.
    pub default_value: f64,
    /// Minimum value, normalized to 0.0 – 1.0.
    pub minimum: f64,
    /// Maximum value, normalized to 0.0 – 1.0.
    pub maximum: f64,
    /// Number of discrete steps, or 0 for continuous expression.
    pub step_count: i32,
    /// Host parameter this expression is associated with — only meaningful when
    /// [`is_associated_parameter_id_valid`](Self::is_associated_parameter_id_valid).
    pub associated_parameter_id: u32,
    /// `NoteExpressionTypeFlags_` bitfield. See [`note_expression_flags`].
    pub flags: i32,
}

impl Default for Vst3NoteExpressionInfo {
    fn default() -> Self {
        Self {
            type_id: 0,
            title: [0; 128],
            short_title: [0; 128],
            units: [0; 128],
            unit_id: 0,
            default_value: 0.0,
            minimum: 0.0,
            maximum: 1.0,
            step_count: 0,
            associated_parameter_id: 0,
            flags: 0,
        }
    }
}

impl Vst3NoteExpressionInfo {
    /// Decoded [`title`](Self::title) as a Rust `String`.
    pub fn title_string(&self) -> String {
        utf16_to_string(&self.title)
    }

    /// Decoded [`short_title`](Self::short_title) as a Rust `String`.
    pub fn short_title_string(&self) -> String {
        utf16_to_string(&self.short_title)
    }

    /// Decoded [`units`](Self::units) as a Rust `String`.
    pub fn units_string(&self) -> String {
        utf16_to_string(&self.units)
    }

    /// True if the expression is bipolar (centred at 0.5, e.g. pan/tuning).
    pub fn is_bipolar(&self) -> bool {
        (self.flags & note_expression_flags::IS_BIPOLAR) != 0
    }

    /// True if the expression is one-shot (sampled at note-on only).
    pub fn is_one_shot(&self) -> bool {
        (self.flags & note_expression_flags::IS_ONE_SHOT) != 0
    }

    /// True if the expression carries an absolute (rather than relative) value.
    pub fn is_absolute(&self) -> bool {
        (self.flags & note_expression_flags::IS_ABSOLUTE) != 0
    }

    /// True if [`associated_parameter_id`](Self::associated_parameter_id) is
    /// meaningful.
    pub fn is_associated_parameter_id_valid(&self) -> bool {
        (self.flags & note_expression_flags::ASSOCIATED_PARAMETER_ID_VALID) != 0
    }

    pub(crate) fn from_c(c: &vst3::Steinberg::Vst::NoteExpressionTypeInfo) -> Self {
        Self {
            type_id: c.typeId,
            title: c.title,
            short_title: c.shortTitle,
            units: c.units,
            unit_id: c.unitId,
            default_value: c.valueDesc.defaultValue,
            minimum: c.valueDesc.minimum,
            maximum: c.valueDesc.maximum,
            step_count: c.valueDesc.stepCount,
            associated_parameter_id: c.associatedParameterId,
            flags: c.flags,
        }
    }
}

/// VST3 `NoteExpressionTypeInfo` flag bits as simple `i32` constants, matching
/// the `flags: i32` field they mask against. Mirrors `NoteExpressionTypeFlags_`
/// from the Steinberg SDK, whose constants are `u32` on unix / `c_int` on
/// Windows — normalised to `i32` here (the bit values fit either way).
// Casts are `u32 as i32` on unix and no-ops on Windows (where the SDK enum is
// already `c_int`); allow the latter's "unnecessary cast" lint.
#[allow(clippy::unnecessary_cast)]
pub mod note_expression_flags {
    use super::NoteExpressionTypeFlags_;

    /// Expression is bipolar (value centred at 0.5).
    pub const IS_BIPOLAR: i32 = NoteExpressionTypeFlags_::kIsBipolar as i32;
    /// Expression is sampled once at note-on (not continuously).
    pub const IS_ONE_SHOT: i32 = NoteExpressionTypeFlags_::kIsOneShot as i32;
    /// Expression value is absolute rather than relative.
    pub const IS_ABSOLUTE: i32 = NoteExpressionTypeFlags_::kIsAbsolute as i32;
    /// `associatedParameterId` is valid.
    pub const ASSOCIATED_PARAMETER_ID_VALID: i32 =
        NoteExpressionTypeFlags_::kAssociatedParameterIDValid as i32;
}

/// Host-facing keyswitch (articulation) descriptor — a flat snake_case view over
/// the vst3 crate's `KeyswitchInfo`, read from
/// `IKeyswitchController::getKeyswitchInfo`. Sample-library instruments use key
/// switches to select articulations (legato / staccato / pizzicato / …); this
/// is the **read** side that tells the host which switches a plugin exposes and
/// on which keys.
#[derive(Clone)]
pub struct Vst3KeyswitchInfo {
    /// Keyswitch kind (`KeyswitchTypeIDs`). See [`keyswitch_type`].
    pub type_id: u32,
    /// UTF-16 display title (the articulation name).
    pub title: [u16; 128],
    /// UTF-16 abbreviated title (for narrow UIs).
    pub short_title: [u16; 128],
    /// Lowest MIDI key that triggers this switch.
    pub keyswitch_min: i32,
    /// Highest MIDI key that triggers this switch.
    pub keyswitch_max: i32,
    /// The key the plugin actually maps the switch to internally (may differ
    /// from the trigger range), or `-1` if not remapped.
    pub key_remapped: i32,
    /// Unit the keyswitch belongs to (for `IUnitInfo` plugins).
    pub unit_id: i32,
    /// `KeyswitchInfo` flags bitfield (no named bits in the current SDK
    /// binding; surfaced verbatim).
    pub flags: i32,
}

impl Default for Vst3KeyswitchInfo {
    fn default() -> Self {
        Self {
            type_id: 0,
            title: [0; 128],
            short_title: [0; 128],
            keyswitch_min: 0,
            keyswitch_max: 0,
            key_remapped: -1,
            unit_id: 0,
            flags: 0,
        }
    }
}

impl Vst3KeyswitchInfo {
    /// Decoded [`title`](Self::title) as a Rust `String`.
    pub fn title_string(&self) -> String {
        utf16_to_string(&self.title)
    }

    /// Decoded [`short_title`](Self::short_title) as a Rust `String`.
    pub fn short_title_string(&self) -> String {
        utf16_to_string(&self.short_title)
    }

    pub(crate) fn from_c(c: &vst3::Steinberg::Vst::KeyswitchInfo) -> Self {
        Self {
            type_id: c.typeId,
            title: c.title,
            short_title: c.shortTitle,
            keyswitch_min: c.keyswitchMin,
            keyswitch_max: c.keyswitchMax,
            key_remapped: c.keyRemapped,
            unit_id: c.unitId,
            flags: c.flags,
        }
    }
}

/// VST3 `KeyswitchTypeIDs` constants — the *kind* of a key switch, mirroring
/// `KeyswitchTypeIDs_` from the Steinberg SDK.
pub mod keyswitch_type {
    use super::KeyswitchTypeIDs_;

    /// Switch selected by playing its key before the note.
    pub const NOTE_ON_KEYSWITCH: u32 = KeyswitchTypeIDs_::kNoteOnKeyswitchTypeID;
    /// Switch that can be changed while a note sustains.
    pub const ON_THE_FLY_KEYSWITCH: u32 = KeyswitchTypeIDs_::kOnTheFlyKeyswitchTypeID;
    /// Switch applied on note release.
    pub const ON_RELEASE_KEYSWITCH: u32 = KeyswitchTypeIDs_::kOnReleaseKeyswitchTypeID;
    /// A key *range* mapped to an articulation rather than a single switch key.
    pub const KEY_RANGE: u32 = KeyswitchTypeIDs_::kKeyRangeTypeID;
}

/// VST3 `PhysicalUITypeIDs` constants — which physical control a
/// note-expression-physical-UI mapping entry refers to. Mirrors
/// `PhysicalUITypeIDs_` from the Steinberg SDK. Returned (as the first tuple
/// element) by
/// [`Vst3Loaded::physical_ui_mapping`](crate::Vst3Loaded::physical_ui_mapping).
///
/// Casts are `u32 as u32` no-ops on unix and `c_int as u32` on Windows; allow
/// the former's lint.
#[allow(clippy::unnecessary_cast)]
pub mod physical_ui_type {
    use super::PhysicalUITypeIDs_;

    /// Horizontal movement of the physical control.
    pub const X_MOVEMENT: u32 = PhysicalUITypeIDs_::kPUIXMovement as u32;
    /// Vertical movement of the physical control.
    pub const Y_MOVEMENT: u32 = PhysicalUITypeIDs_::kPUIYMovement as u32;
    /// Pressure applied to the physical control.
    pub const PRESSURE: u32 = PhysicalUITypeIDs_::kPUIPressure as u32;
    /// Sentinel for "no physical UI type" / unmapped.
    pub const INVALID: u32 = PhysicalUITypeIDs_::kInvalidPUITypeID as u32;
}

/// VST3 `IPrefetchableSupport` result constants, mirroring
/// `ePrefetchableSupport_` from the Steinberg SDK. Returned by
/// [`Vst3Loaded::prefetchable_support`](crate::Vst3Loaded::prefetchable_support).
///
/// Casts are `u32 as u32` no-ops on unix and `c_int as u32` on Windows; allow
/// the former's lint.
#[allow(clippy::unnecessary_cast)]
pub mod prefetchable_support {
    use super::ePrefetchableSupport_;

    /// The plugin can never be used in prefetch (offline look-ahead) mode.
    pub const NEVER: u32 = ePrefetchableSupport_::kIsNeverPrefetchable as u32;
    /// The plugin currently supports prefetch.
    pub const YET: u32 = ePrefetchableSupport_::kIsYetPrefetchable as u32;
    /// The plugin doesn't currently support prefetch (but may later).
    pub const NOT_YET: u32 = ePrefetchableSupport_::kIsNotYetPrefetchable as u32;
}

#[cfg(test)]
mod note_expression_info_tests {
    use super::{note_expression_flags, Vst3NoteExpressionInfo};

    /// Copy a Rust `&str` into a UTF-16, null-terminated `String128` buffer.
    fn string128(s: &str) -> [u16; 128] {
        let mut buf = [0u16; 128];
        for (slot, ch) in buf.iter_mut().zip(s.encode_utf16()) {
            *slot = ch;
        }
        buf
    }

    /// `from_c` flattens `valueDesc`, decodes the UTF-16 title/units, and
    /// surfaces the flag bits through the boolean accessors.
    #[test]
    fn from_c_decodes_fields_and_flags() {
        let mut raw: vst3::Steinberg::Vst::NoteExpressionTypeInfo = unsafe { std::mem::zeroed() };
        raw.typeId = 1; // Pan, conventionally.
        raw.title = string128("Pan");
        raw.units = string128("L/R");
        raw.unitId = 3;
        raw.valueDesc.defaultValue = 0.5;
        raw.valueDesc.minimum = 0.0;
        raw.valueDesc.maximum = 1.0;
        raw.valueDesc.stepCount = 0;
        raw.associatedParameterId = 42;
        // Bipolar + associated-param-id-valid.
        raw.flags = note_expression_flags::IS_BIPOLAR
            | note_expression_flags::ASSOCIATED_PARAMETER_ID_VALID;

        let info = Vst3NoteExpressionInfo::from_c(&raw);

        assert_eq!(info.type_id, 1);
        assert_eq!(info.title_string(), "Pan");
        assert_eq!(info.units_string(), "L/R");
        assert_eq!(info.unit_id, 3);
        assert_eq!(info.default_value, 0.5);
        assert_eq!(info.minimum, 0.0);
        assert_eq!(info.maximum, 1.0);
        assert_eq!(info.step_count, 0);
        assert_eq!(info.associated_parameter_id, 42);

        assert!(info.is_bipolar());
        assert!(info.is_associated_parameter_id_valid());
        assert!(!info.is_one_shot());
        assert!(!info.is_absolute());
    }
}

#[cfg(test)]
mod keyswitch_info_tests {
    use super::{keyswitch_type, Vst3KeyswitchInfo};

    fn string128(s: &str) -> [u16; 128] {
        let mut buf = [0u16; 128];
        for (slot, ch) in buf.iter_mut().zip(s.encode_utf16()) {
            *slot = ch;
        }
        buf
    }

    /// `from_c` decodes the articulation title and trigger key range, and keeps
    /// the keyswitch kind.
    #[test]
    fn from_c_decodes_articulation_and_range() {
        let mut raw: vst3::Steinberg::Vst::KeyswitchInfo = unsafe { std::mem::zeroed() };
        raw.typeId = keyswitch_type::NOTE_ON_KEYSWITCH;
        raw.title = string128("Staccato");
        raw.shortTitle = string128("Stac");
        raw.keyswitchMin = 24; // C1
        raw.keyswitchMax = 24;
        raw.keyRemapped = -1;
        raw.unitId = 0;

        let info = Vst3KeyswitchInfo::from_c(&raw);

        assert_eq!(info.type_id, keyswitch_type::NOTE_ON_KEYSWITCH);
        assert_eq!(info.title_string(), "Staccato");
        assert_eq!(info.short_title_string(), "Stac");
        assert_eq!(info.keyswitch_min, 24);
        assert_eq!(info.keyswitch_max, 24);
        assert_eq!(info.key_remapped, -1);
    }
}
