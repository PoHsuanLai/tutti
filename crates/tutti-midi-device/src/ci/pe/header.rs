//! The Header Data field of a Property Exchange message (M2-103 §7): its three
//! shapes, the closed sets of its enumerated properties, and the reader and
//! writer around them.

use std::num::NonZeroU32;

use super::PeFlowControl;
use super::json;
use super::limits::{MediaType, PeMessage, PropertyName, ResourceId, ResourceName, SubscribeId};
use super::value::{PeExtra, PeValue};
use super::write::Object;
use crate::MaxSysexSize;

/// The most bytes a Header Data field holds, 16383: its length is a 14-bit value
/// (M2-101 §8.7, Table 33).
pub const PE_MAX_HEADER_LEN: usize = 0x3FFF;

/// The bytes of a Property Exchange message besides its Header Data, when it
/// has no Property Data: `F0`, the 13-byte preamble, the Request ID, the 2-byte
/// header length, the three 2-byte chunk and data counts, and `F7` (M2-101
/// Tables 33 and 34). A message with no Property Data is not chunked
/// (M2-101 §8.3.2), so a header fits the Receivable Maximum SysEx Message Size
/// less these.
pub(super) const MESSAGE_OVERHEAD: usize = 1 + 13 + 1 + 2 + 6 + 1;

/// The encoding of Property Data (M2-103 §6.1.6, Table 12).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PeEncoding {
    /// Uncompressed and unencoded; 7-bit data.
    Ascii,
    /// Encoded with [`mcoded7`](super::mcoded7).
    Mcoded7,
    /// Compressed with zlib (RFC 1950), then encoded with Mcoded7.
    ZlibMcoded7,
}

impl PeEncoding {
    /// The property value, as Table 12 spells it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ascii => "ASCII",
            Self::Mcoded7 => "Mcoded7",
            Self::ZlibMcoded7 => "zlib+Mcoded7",
        }
    }

    /// The encoding a property value names, or `None` for any other text.
    #[must_use]
    pub fn new(text: &str) -> Option<Self> {
        [Self::Ascii, Self::Mcoded7, Self::ZlibMcoded7]
            .into_iter()
            .find(|e| e.as_str() == text)
    }
}

/// The `status` of a Reply (§7.4.1, Table 15), and the three codes a Notify
/// message uses (§12).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PeStatus {
    /// 200: success.
    Ok,
    /// 201: a Set Property Data is accepted, with no guarantee of the result.
    Accepted,
    /// 341: the Resource is unavailable or an error occurred; retry.
    Unavailable,
    /// 342: bad data or an unexpected end of the data set; retry.
    BadData,
    /// 343: too many requests; retry.
    TooManyRequests,
    /// 400: the request data is not correct.
    BadRequest,
    /// 403: the reply is not available under the Authorization.
    Forbidden,
    /// 404: the Resource is not supported or found.
    NotFound,
    /// 405: the Resource is not applicable at this time.
    NotAllowed,
    /// 406: Flow Control is not supported.
    FlowControlUnsupported,
    /// 407: Flow Control is required.
    FlowControlRequired,
    /// 413: the payload is too large.
    PayloadTooLarge,
    /// 415: an unsupported media type or encoding.
    UnsupportedMedia,
    /// 445: the data format is a version the receiver cannot support.
    InvalidVersion,
    /// 500: an internal device error.
    InternalError,
    /// 100: Notify, a timeout wait (§12.2.2).
    TimeoutWait,
    /// 144: Notify, terminate the inquiry (§12.1.3).
    Terminate,
    /// 408: Notify, a timeout has occurred (§12.2.4).
    TimedOut,
}

impl PeStatus {
    const ALL: [Self; 18] = [
        Self::Ok,
        Self::Accepted,
        Self::Unavailable,
        Self::BadData,
        Self::TooManyRequests,
        Self::BadRequest,
        Self::Forbidden,
        Self::NotFound,
        Self::NotAllowed,
        Self::FlowControlUnsupported,
        Self::FlowControlRequired,
        Self::PayloadTooLarge,
        Self::UnsupportedMedia,
        Self::InvalidVersion,
        Self::InternalError,
        Self::TimeoutWait,
        Self::Terminate,
        Self::TimedOut,
    ];

    /// The number the property carries.
    #[must_use]
    pub const fn code(self) -> u16 {
        match self {
            Self::Ok => 200,
            Self::Accepted => 201,
            Self::Unavailable => 341,
            Self::BadData => 342,
            Self::TooManyRequests => 343,
            Self::BadRequest => 400,
            Self::Forbidden => 403,
            Self::NotFound => 404,
            Self::NotAllowed => 405,
            Self::FlowControlUnsupported => 406,
            Self::FlowControlRequired => 407,
            Self::PayloadTooLarge => 413,
            Self::UnsupportedMedia => 415,
            Self::InvalidVersion => 445,
            Self::InternalError => 500,
            Self::TimeoutWait => 100,
            Self::Terminate => 144,
            Self::TimedOut => 408,
        }
    }

    /// The status a number names, or `None` for a code the spec does not list.
    #[must_use]
    pub fn new(code: u16) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.code() == code)
    }

    /// What the spec says to do about the status (Table 15).
    #[must_use]
    pub const fn class(self) -> PeStatusClass {
        match self {
            Self::Ok | Self::Accepted => PeStatusClass::Success,
            Self::Unavailable | Self::BadData | Self::TooManyRequests => PeStatusClass::Retry,
            Self::InternalError => PeStatusClass::ServerError,
            Self::TimeoutWait | Self::Terminate | Self::TimedOut => PeStatusClass::Notify,
            Self::BadRequest
            | Self::Forbidden
            | Self::NotFound
            | Self::NotAllowed
            | Self::FlowControlUnsupported
            | Self::FlowControlRequired
            | Self::PayloadTooLarge
            | Self::UnsupportedMedia
            | Self::InvalidVersion => PeStatusClass::ClientError,
        }
    }
}

/// The class of a [`PeStatus`]: Table 15 groups the codes by what the sender of
/// the inquiry does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PeStatusClass {
    /// 200 to 299: do not retry.
    Success,
    /// 300 to 399: the inquiry should be retried.
    Retry,
    /// 400 to 499: the inquiry failed; do not retry.
    ClientError,
    /// 500 to 599: a major error.
    ServerError,
    /// A code a Notify message uses (§12), which no Reply does (§7.4.1).
    Notify,
}

/// The pagination of a List Resource inquiry (§8.6.2): the spec has both
/// properties present or neither.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PePage {
    /// The 0-based position of the first object wanted.
    pub offset: u32,
    /// The most objects wanted, at least 1.
    pub limit: NonZeroU32,
}

/// What a Set Property Data replaces (§10).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PeSetScope {
    /// The whole Property Data.
    Whole,
    /// The properties a Partial Set names (`setPartial`).
    Partial,
}

/// An Inquiry: Get Property Data or Set Property Data header, or the body of a
/// Start Subscription (§7.2, Table 13).
#[derive(Clone, Debug, PartialEq)]
pub struct PeRequest {
    /// The Resource the request targets.
    pub resource: ResourceName,
    /// The entry of the Resource, for one that has several (§8.5).
    pub res_id: Option<ResourceId>,
    /// The encoding wanted of a reply, or that of the Property Data sent.
    pub mutual_encoding: Option<PeEncoding>,
    /// Whether a Get uses Flow Control (`flowControl`; absent is off).
    pub flow_control: PeFlowControl,
    /// The pagination of a List Resource.
    pub page: Option<PePage>,
    /// Whether a Set replaces the whole Property Data or is a Partial Set
    /// (`setPartial`, §10; absent is whole).
    pub set_scope: PeSetScope,
    /// The media type of Property Data that is not JSON (§7.5).
    pub media_type: Option<MediaType>,
    /// The properties this type does not name.
    pub extra: PeExtra,
}

impl PeRequest {
    /// A request for `resource` with no other property.
    #[must_use]
    pub fn new(resource: ResourceName) -> Self {
        Self {
            resource,
            res_id: None,
            mutual_encoding: None,
            flow_control: PeFlowControl::Off,
            page: None,
            set_scope: PeSetScope::Whole,
            media_type: None,
            extra: PeExtra::default(),
        }
    }

    fn read(props: &mut Props) -> Result<Self, PeHeaderError> {
        let resource = props
            .string("resource", ResourceName::new)?
            .ok_or(PeHeaderError::Missing("resource"))?;
        let offset = props.unsigned("offset")?;
        let limit = props.unsigned("limit")?;
        let page = match (offset, limit) {
            (None, None) => None,
            (Some(offset), Some(limit)) => Some(PePage {
                offset,
                limit: NonZeroU32::new(limit).ok_or(PeHeaderError::InvalidValue("limit"))?,
            }),
            (Some(_), None) => return Err(PeHeaderError::Missing("limit")),
            (None, Some(_)) => return Err(PeHeaderError::Missing("offset")),
        };
        Ok(Self {
            resource,
            res_id: props.string("resId", ResourceId::new)?,
            mutual_encoding: props.string("mutualEncoding", PeEncoding::new)?,
            flow_control: match props.boolean("flowControl")? {
                Some(true) => PeFlowControl::On,
                Some(false) | None => PeFlowControl::Off,
            },
            page,
            set_scope: match props.boolean("setPartial")? {
                Some(true) => PeSetScope::Partial,
                Some(false) | None => PeSetScope::Whole,
            },
            media_type: props.string("mediaType", MediaType::new)?,
            extra: PeExtra::default(),
        })
    }

    fn write(&self, out: &mut Object) {
        out.text("resource", self.resource.as_str());
        if let Some(id) = &self.res_id {
            out.text("resId", id.as_str());
        }
        write_encoding(out, self.mutual_encoding);
        if self.flow_control == PeFlowControl::On {
            out.boolean("flowControl", true);
        }
        if let Some(page) = self.page {
            out.integer("offset", page.offset.into());
            out.integer("limit", page.limit.get().into());
        }
        if self.set_scope == PeSetScope::Partial {
            out.boolean("setPartial", true);
        }
        write_media_type(out, self.media_type.as_ref());
        write_extra(out, &self.extra);
    }
}

/// A reply to an inquiry or to a Subscription message (§7.3, Table 14).
#[derive(Clone, Debug, PartialEq)]
pub struct PeReply {
    /// How the inquiry went.
    pub status: PeStatus,
    /// Why, in words, when it did not go well.
    pub message: Option<PeMessage>,
    /// The encoding of the Property Data.
    pub mutual_encoding: Option<PeEncoding>,
    /// The seconds the Property Data may be cached (§7.4.3).
    pub cache_time_secs: Option<u32>,
    /// The objects a List Resource holds, whatever the pagination (§8.6.2).
    pub total_count: Option<u32>,
    /// The Subscription a reply to Start Subscription assigns (§11.1).
    pub subscribe_id: Option<SubscribeId>,
    /// The media type of Property Data that is not JSON (§7.5).
    pub media_type: Option<MediaType>,
    /// The properties this type does not name.
    pub extra: PeExtra,
}

impl PeReply {
    /// A reply with `status` and no other property.
    #[must_use]
    pub fn new(status: PeStatus) -> Self {
        Self {
            status,
            message: None,
            mutual_encoding: None,
            cache_time_secs: None,
            total_count: None,
            subscribe_id: None,
            media_type: None,
            extra: PeExtra::default(),
        }
    }

    fn read(props: &mut Props) -> Result<Self, PeHeaderError> {
        let status = props
            .unsigned("status")?
            .ok_or(PeHeaderError::Missing("status"))?;
        Ok(Self {
            status: u16::try_from(status)
                .ok()
                .and_then(PeStatus::new)
                .ok_or(PeHeaderError::InvalidValue("status"))?,
            message: props.string("message", PeMessage::new)?,
            mutual_encoding: props.string("mutualEncoding", PeEncoding::new)?,
            cache_time_secs: props.unsigned("cacheTime")?,
            total_count: props.unsigned("totalCount")?,
            subscribe_id: props.string("subscribeId", SubscribeId::new)?,
            media_type: props.string("mediaType", MediaType::new)?,
            extra: PeExtra::default(),
        })
    }

    fn write(&self, out: &mut Object) {
        out.integer("status", self.status.code().into());
        if let Some(message) = &self.message {
            out.text("message", message.as_str());
        }
        write_encoding(out, self.mutual_encoding);
        if let Some(secs) = self.cache_time_secs {
            out.integer("cacheTime", secs.into());
        }
        if let Some(count) = self.total_count {
            out.integer("totalCount", count.into());
        }
        if let Some(id) = &self.subscribe_id {
            out.text("subscribeId", id.as_str());
        }
        write_media_type(out, self.media_type.as_ref());
        write_extra(out, &self.extra);
    }
}

/// The commands a Responder sends to update a Subscription (§11.1, Table 39).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PeUpdateCommand {
    /// `partial`: a change to a subset of the Property Data.
    Partial,
    /// `full`: the whole Property Data.
    Full,
    /// `notify`: a request that the Initiator get the Property Data again; no
    /// Property Data follows.
    Notify,
}

impl PeUpdateCommand {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Partial => "partial",
            Self::Full => "full",
            Self::Notify => "notify",
        }
    }
}

/// Who ends a Subscription (§11.1, Table 39, `endedBy`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PeEndedBy {
    /// The Initiator.
    Initiator,
    /// The Responder.
    Responder,
}

impl PeEndedBy {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Initiator => "initiator",
            Self::Responder => "responder",
        }
    }
}

/// A Subscription message header (§11.1, Table 39): the `command` property comes
/// first, and it decides which others the message has.
#[derive(Clone, Debug, PartialEq)]
pub enum PeSubscription {
    /// `start`, from the Initiator: the header is that of a Get, which the
    /// [`PeRequest`] holds, and the Responder assigns the Subscription Id.
    Start(PeRequest),
    /// `partial`, `full` or `notify`, from the Responder.
    Update {
        /// Which update.
        command: PeUpdateCommand,
        /// The Subscription the update belongs to.
        subscribe_id: SubscribeId,
        /// The encoding of the Property Data.
        mutual_encoding: Option<PeEncoding>,
        /// The media type of Property Data that is not JSON (§7.5).
        media_type: Option<MediaType>,
        /// The properties this type does not name.
        extra: PeExtra,
    },
    /// `end`, from either device; no Property Data follows.
    End {
        /// The Subscription that ends.
        subscribe_id: SubscribeId,
        /// Which device ends it.
        ended_by: Option<PeEndedBy>,
        /// The properties this type does not name.
        extra: PeExtra,
    },
}

/// The typed Header Data of a Property Exchange message (M2-103 §7).
///
/// The first property of the JSON object tells the shape: `resource` a
/// request, `status` a reply and `command` a Subscription message (§7.1.1).
#[derive(Clone, Debug, PartialEq)]
pub enum PeHeader {
    /// Inquiry: Get Property Data or Set Property Data.
    Request(PeRequest),
    /// Reply to Get, Set or Subscription.
    Reply(PeReply),
    /// Subscription.
    Subscription(PeSubscription),
}

/// Header Data that is not a header.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PeHeaderError {
    /// The Header Data is longer than its 14-bit length holds.
    #[error("the header is {len} bytes and a header holds at most {PE_MAX_HEADER_LEN}")]
    TooLong {
        /// Its length.
        len: usize,
    },
    /// A byte is not 7-bit ASCII; the Header Data escapes such characters
    /// (M2-103 §6.1.1).
    #[error("byte {at} of the header is not 7-bit ASCII")]
    NotAscii {
        /// The offset of the byte.
        at: usize,
    },
    /// The text is not a one-line JSON object of number, boolean and string
    /// properties (M2-103 §7.1.1).
    #[error(
        "the header is not a one-line JSON object of number, boolean and string properties (at byte {at})"
    )]
    Malformed {
        /// The offset where the grammar stopped.
        at: usize,
    },
    /// A property name is not one (M2-103 §6.1, §7.1.1).
    #[error("{0:?} is not a property name")]
    InvalidName(String),
    /// A property appears twice.
    #[error("the property {0:?} appears twice")]
    Duplicate(String),
    /// The first property is not `resource`, `status` or `command`, or there is none.
    #[error("the first property is not resource, status or command")]
    FirstProperty,
    /// A property the shape requires is missing.
    #[error("the property {0:?} is missing")]
    Missing(&'static str),
    /// A property has a value of the wrong type, or past its limit.
    #[error("the property {0:?} has an invalid value")]
    InvalidValue(&'static str),
    /// A property that belongs to another shape of header.
    #[error("the property {0:?} does not belong in this header")]
    Misplaced(String),
}

/// A header that does not fit the message that carries it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the header is {len} bytes and at most {limit} fit")]
pub struct PeHeaderTooLong {
    /// Its length.
    pub len: usize,
    /// The most that fit.
    pub limit: usize,
}

/// The properties of a header being read, in order, as a set to take from.
struct Props(Vec<(PropertyName, PeValue)>);

impl Props {
    fn new(members: Vec<json::Member>) -> Result<Self, PeHeaderError> {
        let mut props: Vec<(PropertyName, PeValue)> = Vec::with_capacity(members.len());
        for (name, value) in members {
            let name = PropertyName::new(&name).ok_or(PeHeaderError::InvalidName(name))?;
            if props.iter().any(|(n, _)| *n == name) {
                return Err(PeHeaderError::Duplicate(name.into()));
            }
            props.push((name, value));
        }
        Ok(Self(props))
    }

    fn take(&mut self, key: &str) -> Option<PeValue> {
        let at = self.0.iter().position(|(n, _)| n.as_str() == key)?;
        Some(self.0.remove(at).1)
    }

    /// A string property read through `new`.
    fn string<T>(
        &mut self,
        key: &'static str,
        new: impl FnOnce(&str) -> Option<T>,
    ) -> Result<Option<T>, PeHeaderError> {
        match self.take(key) {
            None => Ok(None),
            Some(PeValue::String(s)) => new(&s).map(Some).ok_or(PeHeaderError::InvalidValue(key)),
            Some(_) => Err(PeHeaderError::InvalidValue(key)),
        }
    }

    /// An integer property of at least 0 that fits 32 bits.
    fn unsigned(&mut self, key: &'static str) -> Result<Option<u32>, PeHeaderError> {
        match self.take(key) {
            None => Ok(None),
            Some(PeValue::Integer(n)) => u32::try_from(n)
                .map(Some)
                .map_err(|_| PeHeaderError::InvalidValue(key)),
            Some(_) => Err(PeHeaderError::InvalidValue(key)),
        }
    }

    fn boolean(&mut self, key: &'static str) -> Result<Option<bool>, PeHeaderError> {
        match self.take(key) {
            None => Ok(None),
            Some(PeValue::Bool(b)) => Ok(Some(b)),
            Some(_) => Err(PeHeaderError::InvalidValue(key)),
        }
    }

    /// What is left: the extra properties.
    fn extra(self) -> Result<PeExtra, PeHeaderError> {
        let mut extra = PeExtra::default();
        for (name, value) in self.0 {
            extra
                .insert(name, value)
                .map_err(|e| PeHeaderError::Misplaced(e.0))?;
        }
        Ok(extra)
    }
}

fn write_encoding(out: &mut Object, encoding: Option<PeEncoding>) {
    if let Some(encoding) = encoding {
        out.text("mutualEncoding", encoding.as_str());
    }
}

fn write_media_type(out: &mut Object, media_type: Option<&MediaType>) {
    if let Some(media_type) = media_type {
        out.text("mediaType", media_type.as_str());
    }
}

fn write_extra(out: &mut Object, extra: &PeExtra) {
    for (name, value) in extra.iter() {
        out.value(name.as_str(), value);
    }
}

impl PeHeader {
    /// Reads the Header Data field `bytes`.
    ///
    /// The reader is strict: ASCII only, no whitespace outside a string, no
    /// duplicate property, and each property within its limit. A property the
    /// spec does not name is kept in the header's [`PeExtra`]; one that belongs to
    /// another shape of header is an error.
    ///
    /// # Errors
    ///
    /// A [`PeHeaderError`] naming what broke.
    pub fn parse(bytes: &[u8]) -> Result<Self, PeHeaderError> {
        if bytes.len() > PE_MAX_HEADER_LEN {
            return Err(PeHeaderError::TooLong { len: bytes.len() });
        }
        if let Some(at) = bytes.iter().position(|b| !b.is_ascii()) {
            return Err(PeHeaderError::NotAscii { at });
        }
        let mut props =
            Props::new(json::object(bytes).map_err(|at| PeHeaderError::Malformed { at })?)?;
        let first = props.0.first().map(|(n, _)| n.as_str().to_owned());
        match first.as_deref() {
            Some("resource") => {
                let mut request = PeRequest::read(&mut props)?;
                request.extra = props.extra()?;
                Ok(Self::Request(request))
            }
            Some("status") => {
                let mut reply = PeReply::read(&mut props)?;
                reply.extra = props.extra()?;
                Ok(Self::Reply(reply))
            }
            Some("command") => PeSubscription::read(props).map(Self::Subscription),
            _ => Err(PeHeaderError::FirstProperty),
        }
    }

    /// Writes the Header Data field: compact JSON with the first property the
    /// shape requires, and every character outside printable ASCII escaped.
    ///
    /// `max` is the Receivable Maximum SysEx Message Size of the device the
    /// message goes to. A header has no Property Data to chunk (M2-101 §8.3.2),
    /// so it fits when the whole message does: the header and the 24 bytes of the
    /// message around it, `F0` and `F7` counted, are at most `max`. A header
    /// is also at most [`PE_MAX_HEADER_LEN`].
    ///
    /// # Errors
    ///
    /// [`PeHeaderTooLong`] when it does not fit.
    pub fn encode(&self, max: MaxSysexSize) -> Result<Vec<u8>, PeHeaderTooLong> {
        let mut out = Object::new();
        match self {
            Self::Request(request) => request.write(&mut out),
            Self::Reply(reply) => reply.write(&mut out),
            Self::Subscription(PeSubscription::Start(request)) => {
                out.text("command", "start");
                request.write(&mut out);
            }
            Self::Subscription(PeSubscription::Update {
                command,
                subscribe_id,
                mutual_encoding,
                media_type,
                extra,
            }) => {
                out.text("command", command.as_str());
                out.text("subscribeId", subscribe_id.as_str());
                write_encoding(&mut out, *mutual_encoding);
                write_media_type(&mut out, media_type.as_ref());
                write_extra(&mut out, extra);
            }
            Self::Subscription(PeSubscription::End {
                subscribe_id,
                ended_by,
                extra,
            }) => {
                out.text("command", "end");
                out.text("subscribeId", subscribe_id.as_str());
                if let Some(ended_by) = ended_by {
                    out.text("endedBy", ended_by.as_str());
                }
                write_extra(&mut out, extra);
            }
        }
        let bytes = out.finish();
        let limit = PE_MAX_HEADER_LEN.min((max.get() as usize).saturating_sub(MESSAGE_OVERHEAD));
        if bytes.len() > limit {
            return Err(PeHeaderTooLong {
                len: bytes.len(),
                limit,
            });
        }
        Ok(bytes)
    }
}

impl PeSubscription {
    fn read(mut props: Props) -> Result<Self, PeHeaderError> {
        let command = props
            .string("command", |s| Some(s.to_owned()))?
            .ok_or(PeHeaderError::Missing("command"))?;
        let subscribe_id = |props: &mut Props| {
            props
                .string("subscribeId", SubscribeId::new)?
                .ok_or(PeHeaderError::Missing("subscribeId"))
        };
        match command.as_str() {
            "start" => {
                let mut request = PeRequest::read(&mut props)?;
                request.extra = props.extra()?;
                Ok(Self::Start(request))
            }
            "partial" | "full" | "notify" => Ok(Self::Update {
                command: match command.as_str() {
                    "partial" => PeUpdateCommand::Partial,
                    "full" => PeUpdateCommand::Full,
                    _ => PeUpdateCommand::Notify,
                },
                subscribe_id: subscribe_id(&mut props)?,
                mutual_encoding: props.string("mutualEncoding", PeEncoding::new)?,
                media_type: props.string("mediaType", MediaType::new)?,
                extra: props.extra()?,
            }),
            "end" => Ok(Self::End {
                subscribe_id: subscribe_id(&mut props)?,
                ended_by: props.string("endedBy", |s| match s {
                    "initiator" => Some(PeEndedBy::Initiator),
                    "responder" => Some(PeEndedBy::Responder),
                    _ => None,
                })?,
                extra: props.extra()?,
            }),
            _ => Err(PeHeaderError::InvalidValue("command")),
        }
    }
}
