//! [`PeData`]: the Property Data of a Resource, typed.

use super::super::tree::PeJson;
use super::channels::PeChannelList;
use super::controllers::{PeAllCtrlList, PeChCtrlList, PeCtrlMapList};
use super::entry::PeResourceList;
use super::fields::{PeDataError, PeDataFault};
use super::info::{PeDeviceInfo, PeJsonSchema};
use super::kind::PeResource;
use super::programs::{PeModeList, PeProgramList};
use super::simple::{
    PeBasicChannelRx, PeBasicChannelTx, PeChannelMode, PeCurrentMode, PeExternalSync, PeLocalOn,
};

/// The Property Data of a Resource this crate has no type for: a
/// manufacturer-specific Resource's, or a standard one's that is not modelled.
#[derive(Clone, Debug, PartialEq)]
pub struct PeOpaque {
    resource: PeResource,
    json: PeJson,
}

impl PeOpaque {
    /// The Property Data `json` of `resource`, or `None` when the Resource has
    /// a type, whose [`PeData`] variant holds the data.
    #[must_use]
    pub fn new(resource: PeResource, json: PeJson) -> Option<Self> {
        resource.is_overridable().then_some(Self { resource, json })
    }

    /// The Resource.
    #[must_use]
    pub fn resource(&self) -> &PeResource {
        &self.resource
    }

    /// The Property Data.
    #[must_use]
    pub fn json(&self) -> &PeJson {
        &self.json
    }
}

/// The Property Data of a Resource, as the type of that Resource.
///
/// [`decode`](Self::decode) reads the JSON of a Resource by its name and
/// [`to_json`](Self::to_json) writes it. Each variant's type converts both ways
/// with `From` and `TryFrom`.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum PeData {
    /// ResourceList.
    ResourceList(PeResourceList),
    /// DeviceInfo.
    DeviceInfo(PeDeviceInfo),
    /// ChannelList.
    ChannelList(PeChannelList),
    /// JSONSchema.
    JsonSchema(PeJsonSchema),
    /// ModeList.
    ModeList(PeModeList),
    /// CurrentMode.
    CurrentMode(PeCurrentMode),
    /// ChannelMode.
    ChannelMode(PeChannelMode),
    /// BasicChannelRx.
    BasicChannelRx(PeBasicChannelRx),
    /// BasicChannelTx.
    BasicChannelTx(PeBasicChannelTx),
    /// ProgramList.
    ProgramList(PeProgramList),
    /// LocalOn.
    LocalOn(PeLocalOn),
    /// ExternalSync.
    ExternalSync(PeExternalSync),
    /// AllCtrlList.
    AllCtrlList(PeAllCtrlList),
    /// ChCtrlList.
    ChCtrlList(PeChCtrlList),
    /// CtrlMapList.
    CtrlMapList(PeCtrlMapList),
    /// A Resource with no type here.
    Opaque(PeOpaque),
}

fn items<'a>(resource: &PeResource, json: &'a PeJson) -> Result<&'a [PeJson], PeDataError> {
    json.as_array()
        .ok_or_else(|| PeDataError::new(resource.as_str(), PeDataFault::WrongType("an array")))
}

impl PeData {
    /// The Resource the data belongs to.
    #[must_use]
    pub fn resource(&self) -> PeResource {
        match self {
            Self::ResourceList(_) => PeResource::ResourceList,
            Self::DeviceInfo(_) => PeResource::DeviceInfo,
            Self::ChannelList(_) => PeResource::ChannelList,
            Self::JsonSchema(_) => PeResource::JsonSchema,
            Self::ModeList(_) => PeResource::ModeList,
            Self::CurrentMode(_) => PeResource::CurrentMode,
            Self::ChannelMode(_) => PeResource::ChannelMode,
            Self::BasicChannelRx(_) => PeResource::BasicChannelRx,
            Self::BasicChannelTx(_) => PeResource::BasicChannelTx,
            Self::ProgramList(_) => PeResource::ProgramList,
            Self::LocalOn(_) => PeResource::LocalOn,
            Self::ExternalSync(_) => PeResource::ExternalSync,
            Self::AllCtrlList(_) => PeResource::AllCtrlList,
            Self::ChCtrlList(_) => PeResource::ChCtrlList,
            Self::CtrlMapList(_) => PeResource::CtrlMapList,
            Self::Opaque(opaque) => opaque.resource.clone(),
        }
    }

    /// The Property Data as JSON.
    #[must_use]
    pub fn to_json(&self) -> PeJson {
        match self {
            Self::ResourceList(v) => v.to_json(),
            Self::DeviceInfo(v) => v.to_json(),
            Self::ChannelList(v) => v.to_json(),
            Self::JsonSchema(v) => v.to_json(),
            Self::ModeList(v) => v.to_json(),
            Self::CurrentMode(v) => v.to_json(),
            Self::ChannelMode(v) => v.to_json(),
            Self::BasicChannelRx(v) => v.to_json(),
            Self::BasicChannelTx(v) => v.to_json(),
            Self::ProgramList(v) => v.to_json(),
            Self::LocalOn(v) => v.to_json(),
            Self::ExternalSync(v) => v.to_json(),
            Self::AllCtrlList(v) => v.to_json(),
            Self::ChCtrlList(v) => v.to_json(),
            Self::CtrlMapList(v) => v.to_json(),
            Self::Opaque(v) => v.json.clone(),
        }
    }

    /// Reads the Property Data `json` of `resource`.
    ///
    /// # Errors
    ///
    /// [`PeDataError`] for Property Data that is not the Resource's.
    pub fn decode(resource: &PeResource, json: &PeJson) -> Result<Self, PeDataError> {
        Ok(match resource {
            PeResource::ResourceList => {
                Self::ResourceList(PeResourceList::from_items(items(resource, json)?)?)
            }
            PeResource::DeviceInfo => Self::DeviceInfo(PeDeviceInfo::from_json(json)?),
            PeResource::ChannelList => {
                Self::ChannelList(PeChannelList::from_items(items(resource, json)?)?)
            }
            PeResource::JsonSchema => Self::JsonSchema(PeJsonSchema::from_json(json)?),
            PeResource::ModeList => Self::ModeList(PeModeList::from_items(items(resource, json)?)?),
            PeResource::CurrentMode => Self::CurrentMode(PeCurrentMode::from_json(json)?),
            PeResource::ChannelMode => Self::ChannelMode(PeChannelMode::from_json(json)?),
            PeResource::BasicChannelRx => Self::BasicChannelRx(PeBasicChannelRx::from_json(json)?),
            PeResource::BasicChannelTx => Self::BasicChannelTx(PeBasicChannelTx::from_json(json)?),
            PeResource::ProgramList => {
                Self::ProgramList(PeProgramList::from_items(items(resource, json)?)?)
            }
            PeResource::LocalOn => Self::LocalOn(PeLocalOn::from_json(json)?),
            PeResource::ExternalSync => Self::ExternalSync(PeExternalSync::from_json(json)?),
            PeResource::AllCtrlList => {
                Self::AllCtrlList(PeAllCtrlList::from_items(items(resource, json)?)?)
            }
            PeResource::ChCtrlList => {
                Self::ChCtrlList(PeChCtrlList::from_items(items(resource, json)?)?)
            }
            PeResource::CtrlMapList => {
                Self::CtrlMapList(PeCtrlMapList::from_items(items(resource, json)?)?)
            }
            other => match PeOpaque::new(other.clone(), json.clone()) {
                Some(opaque) => Self::Opaque(opaque),
                None => {
                    return Err(PeDataError::new(
                        other.as_str(),
                        PeDataFault::NotValid("resource"),
                    ));
                }
            },
        })
    }

    /// The pages of a List Resource joined: the entries of each page in order.
    ///
    /// # Errors
    ///
    /// [`PeDataError`] when a page is not an array of entries.
    pub(super) fn decode_pages(
        resource: &PeResource,
        pages: &[PeJson],
    ) -> Result<Self, PeDataError> {
        let mut all = Vec::new();
        for page in pages {
            let page_items = page.as_array().ok_or_else(|| {
                PeDataError::new(resource.as_str(), PeDataFault::WrongType("an array"))
            })?;
            all.extend_from_slice(page_items);
        }
        Self::decode(resource, &PeJson::Array(all))
    }
}

macro_rules! convert {
    ($($variant:ident($ty:ty)),+ $(,)?) => {
        $(
            impl From<$ty> for PeData {
                fn from(value: $ty) -> Self {
                    Self::$variant(value)
                }
            }

            impl TryFrom<PeData> for $ty {
                type Error = PeData;

                fn try_from(data: PeData) -> Result<Self, PeData> {
                    match data {
                        PeData::$variant(value) => Ok(value),
                        other => Err(other),
                    }
                }
            }
        )+
    };
}

convert! {
    ResourceList(PeResourceList),
    DeviceInfo(PeDeviceInfo),
    ChannelList(PeChannelList),
    JsonSchema(PeJsonSchema),
    ModeList(PeModeList),
    CurrentMode(PeCurrentMode),
    ChannelMode(PeChannelMode),
    BasicChannelRx(PeBasicChannelRx),
    BasicChannelTx(PeBasicChannelTx),
    ProgramList(PeProgramList),
    LocalOn(PeLocalOn),
    ExternalSync(PeExternalSync),
    AllCtrlList(PeAllCtrlList),
    ChCtrlList(PeChCtrlList),
    CtrlMapList(PeCtrlMapList),
    Opaque(PeOpaque),
}
