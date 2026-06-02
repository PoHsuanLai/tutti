//! Newtype wrappers over the vst3 crate's `BusInfo` and `ParameterInfo`.

use vst3::Steinberg::Vst::ParameterInfo_::ParameterFlags_;

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
