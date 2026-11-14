//! The names of Resources: the standard ones, the manufacturer-specific ones,
//! and what each standard one declares by default.

use super::super::limits::ResourceName;

/// What a ResourceList entry declares for a Resource when it names nothing
/// (M2-103 §14.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourceDefaults {
    /// Whether the Resource can be read with Get.
    pub can_get: bool,
    /// What Set does with the Resource.
    pub can_set: CanSet,
    /// Whether the Resource can be subscribed to.
    pub can_subscribe: bool,
    /// Whether the Resource, a List Resource, answers `offset` and `limit`.
    pub can_paginate: bool,
    /// Whether every inquiry names an entry of the Resource with `resId`.
    pub require_res_id: bool,
}

/// What Set Property Data may do to a Resource (M2-103 §14.2, Table 79).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CanSet {
    /// `none`: the Resource is read only.
    None,
    /// `full`: the whole Property Data can be set.
    Full,
    /// `partial`: the whole Property Data or a part of it can be set.
    Partial,
}

impl CanSet {
    /// The property value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Full => "full",
            Self::Partial => "partial",
        }
    }

    /// The setting a property value names, or `None` for other text.
    #[must_use]
    pub fn new(text: &str) -> Option<Self> {
        [Self::None, Self::Full, Self::Partial]
            .into_iter()
            .find(|c| c.as_str() == text)
    }
}

/// The name of a manufacturer-specific Resource: `X-` and alphanumeric
/// characters, 36 in all (M2-103 §8.4, Table 13).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CustomResource(ResourceName);

impl CustomResource {
    /// The Resource `text` names, or `None` unless it is a valid name that
    /// starts `X-`.
    #[must_use]
    pub fn new(text: &str) -> Option<Self> {
        text.starts_with("X-")
            .then(|| ResourceName::new(text).map(Self))
            .flatten()
    }

    /// The name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// A Resource of Property Exchange. The standard ones the crate models are
/// variants; the manufacturer's own, `X-` and a name (M2-103 §8.4), are
/// [`Custom`](Self::Custom); a name of any other Resource is
/// [`Unmodelled`](Self::Unmodelled).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PeResource {
    /// `ResourceList`: the Resources a device has (M2-103 §14).
    ResourceList,
    /// `DeviceInfo`: what the device is (M2-105 §3).
    DeviceInfo,
    /// `ChannelList`: the Channels in use (M2-105 §4).
    ChannelList,
    /// JSONSchema: a JSON Schema a ResourceList entry refers to (M2-105 §5).
    JsonSchema,
    /// `ModeList`: the Modes of the device (M2-106 §2).
    ModeList,
    /// `CurrentMode`: the Mode in use (M2-106 §3).
    CurrentMode,
    /// `ChannelMode`: the Omni and Poly/Mono mode of a one-channel device
    /// (M2-108 §2).
    ChannelMode,
    /// `BasicChannelRx`: the Channel received on (M2-108 §3).
    BasicChannelRx,
    /// `BasicChannelTx`: the Channel sent on (M2-108 §4).
    BasicChannelTx,
    /// `ProgramList`: the Programs of a Program Collection (M2-107 §2).
    ProgramList,
    /// `LocalOn`: whether the local input drives the tone generator (M2-109 §2).
    LocalOn,
    /// `ExternalSync`: whether the clock follows external MIDI sync (M2-112 §2).
    ExternalSync,
    /// `AllCtrlList`: the controllers of every Channel (M2-117 §2).
    AllCtrlList,
    /// `ChCtrlList`: the controllers of one Channel of the ChannelList (M2-117 §3).
    ChCtrlList,
    /// `CtrlMapList`: the named values of a controller (M2-117 §4).
    CtrlMapList,
    /// A manufacturer-specific Resource.
    Custom(CustomResource),
    /// A Resource with a name no variant has, for example one of the Device
    /// State resources (M2-111).
    Unmodelled(ResourceName),
}

/// How an inquiry for a Resource names an entry of it (M2-103 §8.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ResIdUse {
    /// Every inquiry carries a `resId`.
    Required,
    /// No inquiry carries a `resId`.
    Unused,
    /// The ResourceList entry says (`requireResId`).
    Declared,
}

const STANDARD: [(PeResource, &str); 15] = [
    (PeResource::ResourceList, "ResourceList"),
    (PeResource::DeviceInfo, "DeviceInfo"),
    (PeResource::ChannelList, "ChannelList"),
    (PeResource::JsonSchema, "JSONSchema"),
    (PeResource::ModeList, "ModeList"),
    (PeResource::CurrentMode, "CurrentMode"),
    (PeResource::ChannelMode, "ChannelMode"),
    (PeResource::BasicChannelRx, "BasicChannelRx"),
    (PeResource::BasicChannelTx, "BasicChannelTx"),
    (PeResource::ProgramList, "ProgramList"),
    (PeResource::LocalOn, "LocalOn"),
    (PeResource::ExternalSync, "ExternalSync"),
    (PeResource::AllCtrlList, "AllCtrlList"),
    (PeResource::ChCtrlList, "ChCtrlList"),
    (PeResource::CtrlMapList, "CtrlMapList"),
];

impl PeResource {
    /// The Resource a name on the wire stands for.
    #[must_use]
    pub fn from_name(name: &ResourceName) -> Self {
        if let Some(custom) = CustomResource::new(name.as_str()) {
            return Self::Custom(custom);
        }
        STANDARD
            .iter()
            .find(|(_, text)| *text == name.as_str())
            .map_or_else(|| Self::Unmodelled(name.clone()), |(r, _)| r.clone())
    }

    /// The name as the wire writes it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Custom(custom) => custom.as_str(),
            Self::Unmodelled(name) => name.as_str(),
            standard => STANDARD
                .iter()
                .find(|(r, _)| r == standard)
                .map_or("", |(_, text)| text),
        }
    }

    /// The name as a header carries it.
    #[must_use]
    pub fn name(&self) -> ResourceName {
        match self {
            Self::Custom(custom) => custom.0.clone(),
            Self::Unmodelled(name) => name.clone(),
            standard => static_name(standard.as_str()),
        }
    }

    /// Whether the Resource is a List Resource: a name that ends `List`
    /// (M2-103 §8.6).
    #[must_use]
    pub fn is_list(&self) -> bool {
        self.as_str().ends_with("List")
    }

    /// Whether a ResourceList entry may declare other than the defaults of this
    /// Resource for `schema`, `requireResId`, `canPaginate`, `canSet` and
    /// `canGet`: only a Resource no AMEI/MMA specification defines may (M2-103
    /// §14.3).
    #[must_use]
    pub fn is_overridable(&self) -> bool {
        matches!(self, Self::Custom(_) | Self::Unmodelled(_))
    }

    /// How an inquiry for the Resource names an entry. `ProgramList`,
    /// `ChCtrlList`, `CtrlMapList` and JSONSchema each have a set of entries
    /// and so need a `resId`.
    #[must_use]
    pub fn res_id_use(&self) -> ResIdUse {
        match self {
            Self::ProgramList | Self::ChCtrlList | Self::CtrlMapList | Self::JsonSchema => {
                ResIdUse::Required
            }
            Self::Custom(_) | Self::Unmodelled(_) => ResIdUse::Declared,
            _ => ResIdUse::Unused,
        }
    }

    /// What a ResourceList entry says when it names nothing.
    #[must_use]
    pub fn defaults(&self) -> ResourceDefaults {
        let base = ResourceDefaults {
            can_get: true,
            can_set: CanSet::None,
            can_subscribe: false,
            can_paginate: false,
            require_res_id: self.res_id_use() == ResIdUse::Required,
        };
        match self {
            Self::CurrentMode
            | Self::BasicChannelRx
            | Self::BasicChannelTx
            | Self::LocalOn
            | Self::ExternalSync => ResourceDefaults {
                can_set: CanSet::Full,
                ..base
            },
            Self::ProgramList => ResourceDefaults {
                can_paginate: true,
                ..base
            },
            _ => base,
        }
    }
}

fn static_name(text: &str) -> ResourceName {
    // Every standard name is a valid name; a test holds it.
    ResourceName::new(text).unwrap_or_else(|| unreachable!("{text} is a resource name"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_standard_name_is_a_valid_resource_name() {
        for (resource, text) in STANDARD {
            assert_eq!(resource.name().as_str(), text);
            assert_eq!(PeResource::from_name(&resource.name()), resource);
        }
    }
}
