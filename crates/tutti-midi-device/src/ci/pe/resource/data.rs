//! [`PeData`]: the Property Data of a Resource, typed.

use super::super::limits::MediaType;
use super::super::tree::{PeJson, PeJsonError};
use super::super::value::PeExtra;
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
use super::state::{PeState, PeStateList};

/// What the Property Data of a Resource without a type holds.
#[derive(Clone, Debug, PartialEq)]
pub enum OpaqueBody {
    /// JSON.
    Json(PeJson),
    /// Data that is not JSON, with the media type its header declares (M2-103
    /// §6.1.5, §7.5).
    Binary {
        /// The media type, which is not `application/json`.
        media_type: MediaType,
        /// The data, before its encoding.
        bytes: Vec<u8>,
    },
}

/// Whether `media_type` is JSON, the media type of Property Data that has none
/// declared (M2-103 §6.1).
fn is_json(media_type: &MediaType) -> bool {
    media_type.as_str().eq_ignore_ascii_case("application/json")
}

/// The Property Data of a Resource this crate has no type for: a
/// manufacturer-specific Resource's, or a standard one's that is not modelled.
#[derive(Clone, Debug, PartialEq)]
pub struct PeOpaque {
    resource: PeResource,
    body: OpaqueBody,
}

impl PeOpaque {
    /// The Property Data `json` of `resource`, or `None` when the Resource has
    /// a type, whose [`PeData`] variant holds the data.
    #[must_use]
    pub fn new(resource: PeResource, json: PeJson) -> Option<Self> {
        resource.is_overridable().then_some(Self {
            resource,
            body: OpaqueBody::Json(json),
        })
    }

    /// The Property Data `bytes` of `resource` that is of `media_type`, or `None`
    /// when the Resource has a type or the media type is JSON, which
    /// [`new`](Self::new) holds.
    #[must_use]
    pub fn binary(resource: PeResource, media_type: MediaType, bytes: Vec<u8>) -> Option<Self> {
        (resource.is_overridable() && !is_json(&media_type)).then_some(Self {
            resource,
            body: OpaqueBody::Binary { media_type, bytes },
        })
    }

    /// The Resource.
    #[must_use]
    pub fn resource(&self) -> &PeResource {
        &self.resource
    }

    /// The Property Data.
    #[must_use]
    pub fn body(&self) -> &OpaqueBody {
        &self.body
    }

    /// The Property Data, when it is JSON.
    #[must_use]
    pub fn json(&self) -> Option<&PeJson> {
        match &self.body {
            OpaqueBody::Json(json) => Some(json),
            OpaqueBody::Binary { .. } => None,
        }
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
    /// StateList.
    StateList(PeStateList),
    /// State.
    State(PeState),
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
            Self::StateList(_) => PeResource::StateList,
            Self::State(_) => PeResource::State,
            Self::Opaque(opaque) => opaque.resource.clone(),
        }
    }

    /// The Property Data as JSON, or `None` for the data of a Resource that is not
    /// JSON.
    #[must_use]
    pub fn to_json(&self) -> Option<PeJson> {
        Some(match self {
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
            Self::StateList(v) => v.to_json(),
            Self::State(v) => match v.body() {
                OpaqueBody::Json(json) => json.clone(),
                OpaqueBody::Binary { .. } => return None,
            },
            Self::Opaque(v) => v.json()?.clone(),
        })
    }

    /// The media type of the data when it is not JSON.
    #[must_use]
    pub fn media_type(&self) -> Option<&MediaType> {
        match self {
            Self::Opaque(PeOpaque {
                body: OpaqueBody::Binary { media_type, .. },
                ..
            }) => Some(media_type),
            Self::State(state) => match state.body() {
                OpaqueBody::Binary { media_type, .. } => Some(media_type),
                OpaqueBody::Json(_) => None,
            },
            _ => None,
        }
    }

    /// The Property Data as it is sent before its encoding: the media type, which is
    /// none for JSON, and the bytes.
    #[must_use]
    pub fn to_wire(&self) -> (Option<MediaType>, Vec<u8>) {
        match self {
            Self::Opaque(PeOpaque {
                body: OpaqueBody::Binary { media_type, bytes },
                ..
            }) => (Some(media_type.clone()), bytes.clone()),
            Self::State(state) => state.to_wire(),
            other => (
                None,
                other.to_json().map(|j| j.encode()).unwrap_or_default(),
            ),
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
            PeResource::StateList => {
                Self::StateList(PeStateList::from_items(items(resource, json)?)?)
            }
            PeResource::State => Self::State(PeState::json(json.clone())),
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

    /// Reads the Property Data `bytes` of `resource` that is of `media_type`: JSON
    /// when the media type is none or `application/json`, and otherwise the
    /// bytes of a Resource that has no type.
    ///
    /// # Errors
    ///
    /// [`PeDataError`] for JSON that is not the Resource's, and for data that is
    /// not JSON of a Resource that is typed.
    pub fn decode_bytes(
        resource: &PeResource,
        media_type: Option<&MediaType>,
        bytes: &[u8],
    ) -> Result<Self, PeDataError> {
        if *resource == PeResource::State {
            return PeState::from_wire(media_type, bytes).map(Self::State);
        }
        match media_type.filter(|m| !is_json(m)) {
            None => {
                let json = PeJson::parse(bytes).map_err(|e| {
                    PeDataError::new(resource.as_str(), PeDataFault::NotValid(json_fault(&e)))
                })?;
                Self::decode(resource, &json)
            }
            Some(media_type) => {
                PeOpaque::binary(resource.clone(), media_type.clone(), bytes.to_vec())
                    .map(Self::Opaque)
                    .ok_or_else(|| {
                        PeDataError::new(resource.as_str(), PeDataFault::NotValid("media type"))
                    })
            }
        }
    }

    /// Writes the properties of a reply's header the data carries: the revision
    /// and time of a state (M2-111 §3.2).
    pub(super) fn write_header(&self, extra: &mut PeExtra) {
        if let Self::State(state) = self {
            state.write_header(extra);
        }
    }

    /// The data with the properties of a reply's header it takes.
    ///
    /// # Errors
    ///
    /// [`PeDataError`] for a property the data cannot take.
    pub(super) fn read_header(self, extra: &PeExtra) -> Result<Self, PeDataError> {
        match self {
            Self::State(state) => state.read_header(extra).map(Self::State),
            other => Ok(other),
        }
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

fn json_fault(_: &PeJsonError) -> &'static str {
    "JSON text"
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
    StateList(PeStateList),
    State(PeState),
    Opaque(PeOpaque),
}
