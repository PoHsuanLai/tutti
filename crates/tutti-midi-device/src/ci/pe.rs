//! The Property Exchange core (M2-103): the Header Data of a Property Exchange
//! message, and Mcoded7, the 8-bit to 7-bit encoding of its Property Data.
//!
//! A Property Exchange message carries a Header Data field, a one-line JSON
//! object in ASCII (M2-103 §7), and a Property Data field. [`PeHeader`] is the
//! typed form of the first: a [`PeRequest`], a [`PeReply`] or a [`PeSubscription`]
//! message, told apart by the property that comes first (§7.1.1). Every limit
//! the spec sets on a property is a type: [`ResourceName`], [`ResourceId`],
//! [`SubscribeId`], [`MediaType`], [`PeMessage`], [`PropertyName`], [`PePage`] and the
//! closed sets [`PeEncoding`], [`PeStatus`], [`PeUpdateCommand`] and [`PeEndedBy`]. A
//! property the spec does not name, a manufacturer-specific `x-` property
//! among them (§6.1.4), is kept in the header's [`PeExtra`] and written back.
//!
//! [`PeHeader::parse`] reads the bytes of a Header Data field and
//! [`PeHeader::encode`] writes them, holding the header to the Receivable Maximum
//! SysEx Message Size from Discovery.
//!
//! The Property Data follows the header, in the [`PeEncoding`] the header names:
//! [`encode_body`] and [`decode_body`] are the codec of each, zlib included, with
//! a [`BodyLimit`] on what a decode may inflate to. [`split_message`] cuts a
//! message into [`PeChunk`]s that each fit the peer's Receivable Maximum SysEx
//! Message Size, and a [`PeReassembler`] collects the chunks of the messages
//! received, from every peer, into a [`PeReceived`] each. What the spec has a
//! receiver send around chunks, the Timeout Wait, the Flow Control acknowledgement,
//! the resend request and the ends of a transaction, is a [`PeSignal`] the
//! reassembler returns and the caller sends as an [`Ack`](crate::Ack) or a
//! [`Nak`](crate::Nak). Nothing reads a clock: time is an argument.
//!
//! [`PeInitiator`] is the Initiator's side over all of it with one peer: the
//! Capabilities exchange (M2-101 §8.5), then Get, Set and Subscribe as a
//! [`PeCall`], each ending in a [`PeOutcome`]. The messages it sends and reads are
//! [`CiMessage::PropertyExchange`](crate::CiMessage::PropertyExchange).
//!
//! The Resources ride on that: [`PeData`] is the Property Data of a standard Resource
//! (ResourceList, DeviceInfo, ChannelList, JSONSchema, ModeList, CurrentMode,
//! ChannelMode, BasicChannelRx and Tx, ProgramList, LocalOn, ExternalSync, AllCtrlList,
//! ChCtrlList, CtrlMapList), typed; [`ResourceInitiator`] makes checked Get, Set and
//! Subscribe calls over [`PeInitiator`], walks paginated lists and keeps the data of a
//! Subscription; [`ResourceResponder`] serves a [`ResourceStore`] and tells subscribers
//! what changes. [`PartialUpdate`] is a Partial Set or `partial` update, applied to the data.
//!
//! ```
//! use tutti_midi_device::{PeHeader, MaxSysexSize, PeRequest, ResourceName};
//!
//! let request = PeRequest::new(ResourceName::new("ChannelList").unwrap());
//! let bytes = PeHeader::Request(request.clone()).encode(MaxSysexSize::of(512)).unwrap();
//! assert_eq!(bytes, br#"{"resource":"ChannelList"}"#);
//! assert_eq!(PeHeader::parse(&bytes), Ok(PeHeader::Request(request)));
//! ```
//!
//! A message sent in chunks, and received:
//!
//! ```
//! use std::time::Instant;
//! use tutti_midi_device::{
//!     BufferLimit, MaxSysexSize, Muid, PeEncoding, PeFlowControl, PeHeader, PeMessageKind,
//!     PeReassembler, PeReassemblyLimits, PeRequest, PeRequestId, PeStep, ResourceName,
//!     StreamLimit, BodyLimit, decode_body, encode_body, split_message,
//! };
//!
//! let header = PeHeader::Request(PeRequest::new(ResourceName::new("State").unwrap()));
//! let body: Vec<u8> = (0..=255).cycle().take(400).collect();
//! let wire = encode_body(PeEncoding::Mcoded7, &body).unwrap();
//! let chunks =
//!     split_message(PeRequestId::of(1), &header, &wire, MaxSysexSize::of(128)).unwrap();
//! assert!(chunks.len() > 1);
//!
//! let mut inbox = PeReassembler::new(PeReassemblyLimits {
//!     streams: StreamLimit::new(4).unwrap(),
//!     bytes: BufferLimit::new(1 << 16).unwrap(),
//! });
//! let (peer, now) = (Muid::of(7), Instant::now());
//! let mut done = None;
//! for chunk in chunks {
//!     let step = inbox.push(peer, PeMessageKind::SET_INQUIRY, chunk, PeFlowControl::Off, now);
//!     if let Ok(PeStep::Complete { message, .. }) = step {
//!         done = Some(message);
//!     }
//! }
//! let message = done.unwrap();
//! assert_eq!(message.header, header);
//! let limit = BodyLimit::new(4096).unwrap();
//! assert_eq!(decode_body(PeEncoding::Mcoded7, &message.body, limit).unwrap(), body);
//! ```
//!
//! [`mcoded7`] holds the encoder and the decoder, pure functions over slices.

mod body;
mod capabilities;
mod chunk;
mod flow;
mod header;
mod initiator;
mod json;
mod limits;
pub mod mcoded7;
mod partial;
mod reassemble;
mod resource;
mod tree;
mod value;
mod write;

pub use body::{BodyLimit, PeBodyError, decode_body, encode_body};
pub use capabilities::{
    PeCapabilities, PeCiMessage, PeVersion, SUB_ID2_PE_CAPABILITIES_INQUIRY,
    SUB_ID2_PE_CAPABILITIES_REPLY, SUB_ID2_PE_NOTIFY, SimultaneousRequests,
};
pub use chunk::{
    ChunkNo, ChunkPosition, ChunkTotal, PE_MAX_CHUNK_DATA_LEN, PeChunk, PeChunkError, PeRequestId,
    split_message,
};
pub use flow::{PE_TIMEOUT, PeAnswer, PeMessageKind, PeSignal, PeTerminator, PeTransaction};
pub use header::{
    PE_MAX_HEADER_LEN, PeEncoding, PeEndedBy, PeHeader, PeHeaderError, PeHeaderTooLong, PePage,
    PeReply, PeRequest, PeSetScope, PeStatus, PeStatusClass, PeSubscription, PeUpdateCommand,
};
pub use initiator::{
    PeAction, PeCall, PeCapabilitiesFailure, PeChange, PeError, PeGet, PeInitiator, PeKind,
    PeNotification, PeOutcome, PeResponse, PeSet, PeSettings, PeSubscribed, PeUnreadable,
};
pub use limits::{
    MediaType, NotInRange, NotValid, PeMessage, PropertyName, ResourceId, ResourceName, SubscribeId,
};
pub use partial::{JsonPointer, PartialError, PartialUpdate};
pub use reassemble::{
    BufferLimit, PeBuffered, PeExpired, PeFlowControl, PeReassembler, PeReassemblyError,
    PeReassemblyLimits, PeReceived, PeRefused, PeStep, StreamLimit,
};
pub use resource::{
    CallError, CallId, CanSet, CcIndex, ChangeKind, ClusterKind, ColumnSource, CtrlControl,
    CtrlKind, CtrlRange, CtrlValueKind, CustomResource, DefaultMapNotCc, EntryError, LinkRole,
    ListedTwice, MAX_LIST_ENTRIES, ModeId, NotVendorName, PaginationFault, ParamPath,
    PeAllCtrlEntry, PeAllCtrlList, PeBankProgram, PeBasicChannelRx, PeBasicChannelTx,
    PeChCtrlEntry, PeChCtrlList, PeChannel, PeChannelEntry, PeChannelList, PeChannelMode,
    PeCluster, PeColumn, PeController, PeCtrl, PeCtrlMapEntry, PeCtrlMapList, PeCurrentMode,
    PeData, PeDataError, PeDataFault, PeDeviceInfo, PeExtensions, PeExternalSync, PeJsonSchema,
    PeLink, PeLocalOn, PeMidiMode, PeModeEntry, PeModeList, PeOpaque, PeProgramEntry,
    PeProgramList, PeResource, PeResourceList, PeText, Priority, PublishError, RequestRefused,
    ResIdUse, ResourceAction, ResourceDefaults, ResourceEntry, ResourceEvent, ResourceFailure,
    ResourceGet, ResourceInitiator, ResourceResponder, ResourceSet, ResourceStore, ResourceTarget,
    ResponderAction, ResponderEvent, ResponderSettings, SetBody, SignificantBits, StepCount,
    StoreError, TypeHint, UpdateKind,
};
pub use tree::{PE_JSON_MAX_DEPTH, PeJson, PeJsonError, PeObject};
pub use value::{PeDecimal, PeExtra, PeReservedProperty, PeValue};

#[cfg(test)]
mod tests;
