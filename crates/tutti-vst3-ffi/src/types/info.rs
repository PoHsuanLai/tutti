//! Newtype wrappers over the vst3 crate's `BusInfo`, `ParameterInfo`, and
//! `NoteExpressionTypeInfo`.

use vst3::Steinberg::Vst::IAutomationState_::AutomationStates_;
use vst3::Steinberg::Vst::KeyswitchTypeIDs_;
use vst3::Steinberg::Vst::NoteExpressionTypeInfo_::NoteExpressionTypeFlags_;
use vst3::Steinberg::Vst::ParameterInfo_::ParameterFlags_;
use vst3::Steinberg::Vst::{PhysicalUITypeIDs_, ePrefetchableSupport_};

use tutti_plugin_types::AutomationMode;

use crate::sdk::utf16_to_string;

/// Host-facing bus descriptor. Thin wrapper around `vst3::Steinberg::Vst::BusInfo`
/// whose field layout is retained verbatim for FFI, but whose only public
/// accessor for the UTF-16 name is [`BusInfo::name_string`].
#[derive(Clone)]
pub struct BusInfo {
    pub(crate) inner: vst3::Steinberg::Vst::BusInfo,
}

opaque_debug!(BusInfo);

impl Default for BusInfo {
    fn default() -> Self {
        Self {
            inner: crate::sdk::zeroed(),
        }
    }
}

impl BusInfo {
    /// What the bus carries, or `None` for a media type the SDK does not
    /// define.
    pub fn media_type(&self) -> Option<crate::MediaType> {
        crate::MediaType::from_raw(self.inner.mediaType)
    }

    /// Which way the bus faces, or `None` for a direction the SDK does not
    /// define.
    pub fn direction(&self) -> Option<crate::BusDirection> {
        crate::BusDirection::from_raw(self.inner.direction)
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

    /// The raw record, for the plugin to fill in.
    pub(crate) fn as_mut_inner(&mut self) -> &mut vst3::Steinberg::Vst::BusInfo {
        &mut self.inner
    }
}

/// Host-facing parameter descriptor — a flat snake_case view over the vst3
/// crate's `ParameterInfo`, populated from the C struct returned by
/// `IEditController::getParameterInfo`.
#[derive(Clone, Debug)]
pub struct Vst3ParameterInfo {
    id: u32,
    title: [u16; 128],
    short_title: [u16; 128],
    units: [u16; 128],
    step_count: i32,
    default_normalized_value: f64,
    unit_id: i32,
    flags: ParameterFlags,
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
            flags: ParameterFlags::empty(),
        }
    }
}

impl Vst3ParameterInfo {
    /// Stable parameter id used in automation messages.
    #[must_use]
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Number of discrete steps, or 0 for continuous parameters.
    #[must_use]
    pub fn step_count(&self) -> i32 {
        self.step_count
    }

    /// Default value, normalized to 0.0 – 1.0.
    #[must_use]
    pub fn default_normalized_value(&self) -> f64 {
        self.default_normalized_value
    }

    /// Unit the parameter belongs to (for `IUnitInfo` plugins).
    #[must_use]
    pub fn unit_id(&self) -> i32 {
        self.unit_id
    }

    /// The parameter's flags.
    #[must_use]
    pub fn flags(&self) -> ParameterFlags {
        self.flags
    }

    /// The title as a Rust `String`.
    pub fn title_string(&self) -> String {
        utf16_to_string(&self.title)
    }

    /// The short title as a Rust `String`.
    pub fn short_title_string(&self) -> String {
        utf16_to_string(&self.short_title)
    }

    /// The units as a Rust `String`.
    pub fn units_string(&self) -> String {
        utf16_to_string(&self.units)
    }

    /// True if the parameter carries the `kCanAutomate` flag.
    pub fn can_automate(&self) -> bool {
        self.flags.contains(ParameterFlags::CAN_AUTOMATE)
    }

    /// True if the parameter is read-only.
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        self.flags.contains(ParameterFlags::IS_READ_ONLY)
    }

    /// True if the parameter should be hidden from the host UI.
    #[must_use]
    pub fn is_hidden(&self) -> bool {
        self.flags.contains(ParameterFlags::IS_HIDDEN)
    }

    /// True if the parameter is the bypass parameter.
    #[must_use]
    pub fn is_bypass(&self) -> bool {
        self.flags.contains(ParameterFlags::IS_BYPASS)
    }

    /// True if writing this parameter selects a program.
    ///
    /// VST3 has no load-preset call: a program is chosen by writing the
    /// parameter carrying this flag, whose normalized value maps onto
    /// `[0, program_count)` in the unit's program list. This is the *only*
    /// route, which is why the flag has to survive the boundary rather than
    /// stopping at the format layer.
    #[must_use]
    pub fn is_program_change(&self) -> bool {
        self.flags.contains(ParameterFlags::IS_PROGRAM_CHANGE)
    }

    /// True if the parameter wraps around at its extremes.
    #[must_use]
    pub fn is_wrap(&self) -> bool {
        self.flags.contains(ParameterFlags::IS_WRAP)
    }

    /// Reads the plugin's `ParameterInfo` record.
    pub(crate) fn from_c(c: &vst3::Steinberg::Vst::ParameterInfo) -> Self {
        Self {
            id: c.id,
            title: c.title,
            short_title: c.shortTitle,
            units: c.units,
            step_count: c.stepCount,
            default_normalized_value: c.defaultNormalizedValue,
            unit_id: c.unitId,
            flags: ParameterFlags::from_bits_retain(c.flags),
        }
    }
}

bitflags::bitflags! {
    /// `ParameterInfo::flags`, mirroring `ParameterFlags_` from the Steinberg
    /// SDK. A set the plugin filled in keeps bits this host does not name.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct ParameterFlags: i32 {
        /// Parameter can be automated by the host.
        const CAN_AUTOMATE = ParameterFlags_::kCanAutomate;
        /// Parameter is read-only (display-only, cannot be edited).
        const IS_READ_ONLY = ParameterFlags_::kIsReadOnly;
        /// Parameter wraps around at its extremes (e.g. phase).
        const IS_WRAP = ParameterFlags_::kIsWrapAround;
        /// Parameter is a discrete list (maps to `stepCount` entries).
        const IS_LIST = ParameterFlags_::kIsList;
        /// Parameter should be hidden from host UI.
        const IS_HIDDEN = ParameterFlags_::kIsHidden;
        /// Parameter controls a program change.
        const IS_PROGRAM_CHANGE = ParameterFlags_::kIsProgramChange;
        /// Parameter is the plugin's bypass switch.
        const IS_BYPASS = ParameterFlags_::kIsBypass;
        // The plugin's own bits beyond the ones named.
        const _ = !0;
    }
}

bitflags::bitflags! {
    /// What the host tells a plugin about automation, by `IAutomationState`'s
    /// `AutomationStates_`: reading, writing, both or (empty) neither. Pass to
    /// [`Plugin::set_automation_state`](crate::Plugin::set_automation_state).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct AutomationStates: i32 {
        /// Host is reading automation.
        const READ = AutomationStates_::kReadState;
        /// Host is writing automation.
        const WRITE = AutomationStates_::kWriteState;
    }
}

impl From<AutomationMode> for AutomationStates {
    /// Encodes the format-neutral [`AutomationMode`] as the VST3 bitmask. The
    /// mapping lives here so `tutti-plugin-types` stays format-agnostic.
    fn from(mode: AutomationMode) -> Self {
        match mode {
            AutomationMode::Off => Self::empty(),
            AutomationMode::Reading => Self::READ,
            AutomationMode::Writing => Self::WRITE,
            AutomationMode::ReadWriting => Self::READ | Self::WRITE,
        }
    }
}

impl AutomationStates {
    /// The [`AutomationMode`] these bits name (inverse of `From`).
    #[must_use]
    pub fn mode(self) -> AutomationMode {
        match (self.contains(Self::READ), self.contains(Self::WRITE)) {
            (false, false) => AutomationMode::Off,
            (true, false) => AutomationMode::Reading,
            (false, true) => AutomationMode::Writing,
            (true, true) => AutomationMode::ReadWriting,
        }
    }
}

#[cfg(test)]
mod automation_states_tests {
    use super::*;

    #[test]
    fn mode_bits_round_trip() {
        for mode in [
            AutomationMode::Off,
            AutomationMode::Reading,
            AutomationMode::Writing,
            AutomationMode::ReadWriting,
        ] {
            assert_eq!(AutomationStates::from(mode).mode(), mode);
        }
    }

    #[test]
    fn mode_bit_values() {
        // 0=none, 1=read, 2=write, 3=read|write (the SDK values).
        assert_eq!(AutomationStates::from(AutomationMode::Off).bits(), 0);
        assert_eq!(AutomationStates::from(AutomationMode::Reading).bits(), 1);
        assert_eq!(AutomationStates::from(AutomationMode::Writing).bits(), 2);
        assert_eq!(
            AutomationStates::from(AutomationMode::ReadWriting).bits(),
            3
        );
    }
}

/// Host-facing note-expression-type descriptor — a flat snake_case view over the
/// vst3 crate's `NoteExpressionTypeInfo`, read from the plugin's
/// `INoteExpressionController::getNoteExpressionInfo`. This is the **read** side
/// of note expression: it tells the host which per-note expression dimensions a
/// plugin supports (the `tutti_plugin_types::NoteExpressionValue` send side is separate).
#[derive(Clone, Debug)]
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
    /// The expression's flags.
    pub flags: NoteExpressionFlags,
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
            flags: NoteExpressionFlags::empty(),
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
    #[must_use]
    pub fn is_bipolar(&self) -> bool {
        self.flags.contains(NoteExpressionFlags::IS_BIPOLAR)
    }

    /// True if the expression is one-shot (sampled at note-on only).
    #[must_use]
    pub fn is_one_shot(&self) -> bool {
        self.flags.contains(NoteExpressionFlags::IS_ONE_SHOT)
    }

    /// True if the expression carries an absolute (rather than relative) value.
    #[must_use]
    pub fn is_absolute(&self) -> bool {
        self.flags.contains(NoteExpressionFlags::IS_ABSOLUTE)
    }

    /// True if [`associated_parameter_id`](Self::associated_parameter_id) is
    /// meaningful.
    #[must_use]
    pub fn is_associated_parameter_id_valid(&self) -> bool {
        self.flags
            .contains(NoteExpressionFlags::ASSOCIATED_PARAMETER_ID_VALID)
    }

    /// Reads the plugin's `NoteExpressionTypeInfo` record.
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
            flags: NoteExpressionFlags::from_bits_retain(c.flags),
        }
    }
}

bitflags::bitflags! {
    /// `NoteExpressionTypeInfo::flags`, mirroring `NoteExpressionTypeFlags_`
    /// from the Steinberg SDK.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct NoteExpressionFlags: i32 {
        /// Expression is bipolar (value centred at 0.5).
        const IS_BIPOLAR = crate::sdk_enum_i32(NoteExpressionTypeFlags_::kIsBipolar);
        /// Expression is sampled once at note-on (not continuously).
        const IS_ONE_SHOT = crate::sdk_enum_i32(NoteExpressionTypeFlags_::kIsOneShot);
        /// Expression value is absolute rather than relative.
        const IS_ABSOLUTE = crate::sdk_enum_i32(NoteExpressionTypeFlags_::kIsAbsolute);
        /// `associatedParameterId` is valid.
        const ASSOCIATED_PARAMETER_ID_VALID =
            crate::sdk_enum_i32(NoteExpressionTypeFlags_::kAssociatedParameterIDValid);
        // The plugin's own bits beyond the ones named.
        const _ = !0;
    }
}

/// Host-facing keyswitch (articulation) descriptor — a flat snake_case view over
/// the vst3 crate's `KeyswitchInfo`, read from
/// `IKeyswitchController::getKeyswitchInfo`. Sample-library instruments use key
/// switches to select articulations (legato / staccato / pizzicato / …); this
/// is the **read** side that tells the host which switches a plugin exposes and
/// on which keys.
#[derive(Clone, Debug)]
pub struct Vst3KeyswitchInfo {
    /// What kind of switch this is.
    pub kind: KeyswitchKind,
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
            kind: KeyswitchKind::from_raw(0),
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

    /// Reads the plugin's `KeyswitchInfo` record.
    pub(crate) fn from_c(c: &vst3::Steinberg::Vst::KeyswitchInfo) -> Self {
        Self {
            kind: KeyswitchKind::from_raw(c.typeId),
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

/// One entry in a plugin's unit tree, read from `IUnitInfo::getUnitInfo`.
///
/// A *unit* groups parameters, programs and note-expression entries — a
/// multitimbral instrument exposes one per part, an effect rack one per slot.
/// Units nest, so [`parent`](Self::parent) is what gives the tree its shape.
///
/// **This struct decodes rather than mirrors**, unlike its siblings above. The
/// VST3 struct spells "no parent" and "no program list" as `-1` and its name as
/// a fixed UTF-16 buffer; all three are decoded once here, at `from_c`, so a
/// caller cannot read a sentinel as an id or forget to stop at the NUL. The
/// sentinels are not part of the vocabulary a host should have to know.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vst3UnitInfo {
    /// This unit's id. `0` is the root unit, which every plugin has implicitly.
    pub id: i32,
    /// The unit this one hangs off, or `None` for the root (`kNoParentUnitId`).
    pub parent: Option<i32>,
    /// Display name, already decoded from UTF-16.
    pub name: String,
    /// The program list this unit selects programs from, or `None` when it has
    /// none (`kNoProgramListId`) **or** when the id names no list the plugin
    /// actually publishes.
    ///
    /// Those two are deliberately collapsed. A dangling id is a plugin bug —
    /// several in the mda set carry one — and reporting it as `Some` would
    /// invite a caller to ask for programs that cannot be enumerated. The
    /// question this field answers is "can I show programs for this unit", and
    /// for a dangling id the answer is no. Use
    /// [`program_list_id_raw`](Self::program_list_id_raw) to see the plugin's
    /// unfiltered claim.
    pub program_list: Option<i32>,
    /// The `programListId` exactly as the plugin reported it, before the
    /// resolution that produces [`program_list`](Self::program_list). Kept so a
    /// diagnostic can tell "no list" from "a list that does not exist".
    pub program_list_id_raw: i32,
}

impl Vst3UnitInfo {
    /// True when this unit named a program list the plugin does not publish.
    /// Always `false` for a unit that simply has no list.
    #[must_use]
    pub fn has_dangling_program_list(&self) -> bool {
        self.program_list.is_none() && self.program_list_id_raw != unit_ids::NO_PROGRAM_LIST
    }

    /// Decodes the C struct. `program_list` is left unresolved (`None` unless
    /// the raw id is a real one); [`resolve_program_list`](Self::resolve_program_list)
    /// finishes the job once the list ids are known.
    /// Reads the plugin's `UnitInfo` record.
    pub(crate) fn from_c(c: &vst3::Steinberg::Vst::UnitInfo) -> Self {
        Self {
            id: c.id,
            parent: (c.parentUnitId != unit_ids::NO_PARENT).then_some(c.parentUnitId),
            name: utf16_to_string(&c.name),
            program_list: None,
            program_list_id_raw: c.programListId,
        }
    }

    /// Second half of the decode: keep `program_list` only if `known` contains
    /// it. Split from `from_c` because resolving needs every list id, which is
    /// a second call the accessor makes once for the whole enumeration rather
    /// than once per unit.
    pub(crate) fn resolve_program_list(&mut self, known: &[i32]) {
        self.program_list = (self.program_list_id_raw != unit_ids::NO_PROGRAM_LIST
            && known.contains(&self.program_list_id_raw))
        .then_some(self.program_list_id_raw);
    }
}

/// A named set of programs (presets) a unit can select from, read from
/// `IUnitInfo::getProgramListInfo`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vst3ProgramListInfo {
    /// This list's id, as referenced by [`Vst3UnitInfo::program_list`].
    pub id: i32,
    /// Display name, already decoded from UTF-16.
    pub name: String,
    /// How many programs the list holds — the exclusive upper bound of the
    /// program-index space. Clamped at zero: a negative count is meaningless
    /// and would otherwise become an enormous `u32`.
    pub program_count: u32,
}

impl Vst3ProgramListInfo {
    /// Reads the plugin's `ProgramListInfo` record.
    pub(crate) fn from_c(c: &vst3::Steinberg::Vst::ProgramListInfo) -> Self {
        Self {
            id: c.id,
            name: utf16_to_string(&c.name),
            program_count: c.programCount.max(0) as u32,
        }
    }
}

/// VST3 unit-id sentinels, mirroring the SDK's `kRootUnitId` /
/// `kNoParentUnitId` / `kNoProgramListId`.
///
/// [`Vst3UnitInfo`] decodes the two negative ones into `Option`, so a caller
/// normally never sees them. They are public for the case that still needs the
/// raw vocabulary: passing a unit id *back* to the plugin, where
/// [`ROOT`](self::unit_ids::ROOT) is a legitimate argument.
pub mod unit_ids {
    /// The implicit top-level unit every plugin has.
    pub const ROOT: i32 = vst3::Steinberg::Vst::kRootUnitId;
    /// `parentUnitId` of the root unit — decoded as `None`.
    pub const NO_PARENT: i32 = vst3::Steinberg::Vst::kNoParentUnitId;
    /// `programListId` of a unit with no programs — decoded as `None`.
    pub const NO_PROGRAM_LIST: i32 = vst3::Steinberg::Vst::kNoProgramListId;
}

/// The kind of a key switch, by the SDK's `KeyswitchTypeIDs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyswitchKind {
    /// Switch selected by playing its key before the note.
    NoteOn,
    /// Switch that can be changed while a note sustains.
    OnTheFly,
    /// Switch applied on note release.
    OnRelease,
    /// A key *range* mapped to an articulation rather than a single switch key.
    KeyRange,
    /// A kind the SDK does not define, as the plugin sent it.
    Other(u32),
}

impl KeyswitchKind {
    /// The kind a `KeyswitchTypeIDs` value names.
    #[must_use]
    pub fn from_raw(raw: u32) -> Self {
        match raw {
            KeyswitchTypeIDs_::kNoteOnKeyswitchTypeID => Self::NoteOn,
            KeyswitchTypeIDs_::kOnTheFlyKeyswitchTypeID => Self::OnTheFly,
            KeyswitchTypeIDs_::kOnReleaseKeyswitchTypeID => Self::OnRelease,
            KeyswitchTypeIDs_::kKeyRangeTypeID => Self::KeyRange,
            other => Self::Other(other),
        }
    }
}

/// Which physical control a note-expression-physical-UI mapping entry refers
/// to, by the SDK's `PhysicalUITypeIDs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PhysicalUiType {
    /// Horizontal movement of the physical control.
    XMovement,
    /// Vertical movement of the physical control.
    YMovement,
    /// Pressure applied to the physical control.
    Pressure,
    /// A type the SDK does not define (including its "invalid" sentinel), as
    /// the plugin sent it.
    Other(u32),
}

impl PhysicalUiType {
    /// The type a `PhysicalUITypeIDs` value names.
    #[must_use]
    pub fn from_raw(raw: u32) -> Self {
        let is = |sdk| raw == crate::sdk_enum_u32(sdk);
        if is(PhysicalUITypeIDs_::kPUIXMovement) {
            Self::XMovement
        } else if is(PhysicalUITypeIDs_::kPUIYMovement) {
            Self::YMovement
        } else if is(PhysicalUITypeIDs_::kPUIPressure) {
            Self::Pressure
        } else {
            Self::Other(raw)
        }
    }

    /// The `PhysicalUITypeIDs` value.
    #[must_use]
    pub fn raw(self) -> u32 {
        match self {
            Self::XMovement => crate::sdk_enum_u32(PhysicalUITypeIDs_::kPUIXMovement),
            Self::YMovement => crate::sdk_enum_u32(PhysicalUITypeIDs_::kPUIYMovement),
            Self::Pressure => crate::sdk_enum_u32(PhysicalUITypeIDs_::kPUIPressure),
            Self::Other(raw) => raw,
        }
    }
}

/// A plugin's answer to `IPrefetchableSupport`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefetchSupport {
    /// The plugin can never be used in prefetch (offline look-ahead) mode.
    Never,
    /// The plugin supports prefetch now.
    Yes,
    /// The plugin does not support prefetch now, but may later.
    NotYet,
    /// A value the SDK does not define, as returned.
    Unknown(u32),
}

impl PrefetchSupport {
    /// The `ePrefetchableSupport` value the plugin returned.
    #[must_use]
    pub fn from_raw(raw: u32) -> Self {
        let is = |sdk| raw == crate::sdk_enum_u32(sdk);
        if is(ePrefetchableSupport_::kIsNeverPrefetchable) {
            Self::Never
        } else if is(ePrefetchableSupport_::kIsYetPrefetchable) {
            Self::Yes
        } else if is(ePrefetchableSupport_::kIsNotYetPrefetchable) {
            Self::NotYet
        } else {
            Self::Unknown(raw)
        }
    }
}

#[cfg(test)]
mod note_expression_info_tests {
    use super::{NoteExpressionFlags, Vst3NoteExpressionInfo};

    /// Copies a Rust `&str` into a UTF-16, null-terminated `String128` buffer.
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
        // SAFETY: all-zero bytes is a valid value of this plain-data SDK struct.
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
        raw.flags = (NoteExpressionFlags::IS_BIPOLAR
            | NoteExpressionFlags::ASSOCIATED_PARAMETER_ID_VALID)
            .bits();

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
    use super::{KeyswitchKind, Vst3KeyswitchInfo};
    use vst3::Steinberg::Vst::KeyswitchTypeIDs_;

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
        // SAFETY: all-zero bytes is a valid value of this plain-data SDK struct.
        let mut raw: vst3::Steinberg::Vst::KeyswitchInfo = unsafe { std::mem::zeroed() };
        raw.typeId = KeyswitchTypeIDs_::kNoteOnKeyswitchTypeID;
        raw.title = string128("Staccato");
        raw.shortTitle = string128("Stac");
        raw.keyswitchMin = 24; // C1
        raw.keyswitchMax = 24;
        raw.keyRemapped = -1;
        raw.unitId = 0;

        let info = Vst3KeyswitchInfo::from_c(&raw);

        assert_eq!(info.kind, KeyswitchKind::NoteOn);
        assert_eq!(info.title_string(), "Staccato");
        assert_eq!(info.short_title_string(), "Stac");
        assert_eq!(info.keyswitch_min, 24);
        assert_eq!(info.keyswitch_max, 24);
        assert_eq!(info.key_remapped, -1);
    }
}

#[cfg(test)]
mod unit_info_tests {
    use super::{Vst3ProgramListInfo, Vst3UnitInfo, unit_ids};

    fn string128(s: &str) -> [u16; 128] {
        let mut buf = [0u16; 128];
        for (slot, ch) in buf.iter_mut().zip(s.encode_utf16()) {
            *slot = ch;
        }
        buf
    }

    fn raw_unit(
        id: i32,
        parent: i32,
        name: &str,
        program_list: i32,
    ) -> vst3::Steinberg::Vst::UnitInfo {
        // SAFETY: all-zero bytes is a valid value of this plain-data SDK struct.
        let mut raw: vst3::Steinberg::Vst::UnitInfo = unsafe { std::mem::zeroed() };
        raw.id = id;
        raw.parentUnitId = parent;
        raw.name = string128(name);
        raw.programListId = program_list;
        raw
    }

    /// The root's `kNoParentUnitId` becomes `None` rather than travelling as a
    /// `-1` a caller could mistake for a unit id.
    #[test]
    fn the_root_units_absent_parent_decodes_to_none() {
        let info = Vst3UnitInfo::from_c(&raw_unit(
            unit_ids::ROOT,
            unit_ids::NO_PARENT,
            "Root",
            unit_ids::NO_PROGRAM_LIST,
        ));

        assert_eq!(info.id, 0);
        assert_eq!(info.parent, None);
        assert_eq!(info.name, "Root");
    }

    /// A nested unit keeps its parent, and the name stops at the NUL rather
    /// than carrying 128 code units of padding.
    #[test]
    fn a_nested_unit_keeps_its_parent_and_trims_its_name() {
        let mut info = Vst3UnitInfo::from_c(&raw_unit(7, 0, "Part 2", 100));
        info.resolve_program_list(&[100]);

        assert_eq!(info.parent, Some(0));
        assert_eq!(info.name, "Part 2");
        assert_eq!(
            info.name.len(),
            6,
            "the UTF-16 padding leaked into the name"
        );
        assert_eq!(info.program_list, Some(100));
    }

    /// A unit naming a program list the plugin never publishes reports `None`,
    /// so a caller cannot ask for programs that cannot be enumerated. Several
    /// plugins in the mda set do exactly this.
    #[test]
    fn a_dangling_program_list_id_does_not_survive_resolution() {
        let mut info = Vst3UnitInfo::from_c(&raw_unit(1, 0, "Orphan", 999));
        info.resolve_program_list(&[100, 200]);

        assert_eq!(info.program_list, None);
        assert!(
            info.has_dangling_program_list(),
            "a dangling id must stay distinguishable from having no list at all"
        );
        assert_eq!(info.program_list_id_raw, 999);
    }

    /// Having no program list is not the same as naming one that is missing,
    /// even though both read as `None`.
    #[test]
    fn no_program_list_is_not_reported_as_dangling() {
        let mut info = Vst3UnitInfo::from_c(&raw_unit(1, 0, "Plain", unit_ids::NO_PROGRAM_LIST));
        info.resolve_program_list(&[100]);

        assert_eq!(info.program_list, None);
        assert!(!info.has_dangling_program_list());
    }

    /// A negative `programCount` cannot become an enormous `u32` and drive a
    /// caller's enumeration loop.
    #[test]
    fn a_negative_program_count_clamps_to_zero() {
        // SAFETY: all-zero bytes is a valid value of this plain-data SDK struct.
        let mut raw: vst3::Steinberg::Vst::ProgramListInfo = unsafe { std::mem::zeroed() };
        raw.id = 100;
        raw.name = string128("Presets");
        raw.programCount = -1;

        let info = Vst3ProgramListInfo::from_c(&raw);

        assert_eq!(info.program_count, 0);
        assert_eq!(info.name, "Presets");
    }
}
