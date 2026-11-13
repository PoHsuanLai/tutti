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
//! let (peer, now) = (Muid::new(7).unwrap(), Instant::now());
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
mod chunk;
mod flow;
mod header;
mod json;
mod limits;
pub mod mcoded7;
mod reassemble;
mod value;
mod write;

pub use body::{BodyLimit, PeBodyError, decode_body, encode_body};
pub use chunk::{
    ChunkNo, ChunkPosition, ChunkTotal, PE_MAX_CHUNK_DATA_LEN, PeChunk, PeChunkError, PeRequestId,
    split_message,
};
pub use flow::{
    PE_TIMEOUT, PeAnswer, PeMessageKind, PeSignal, PeTerminator, PeTransaction, TimeoutWait,
};
pub use header::{
    PE_MAX_HEADER_LEN, PeEncoding, PeEndedBy, PeHeader, PeHeaderError, PeHeaderTooLong, PePage,
    PeReply, PeRequest, PeStatus, PeSubscription, PeUpdateCommand,
};
pub use limits::{
    MediaType, NotInRange, NotValid, PeMessage, PropertyName, ResourceId, ResourceName, SubscribeId,
};
pub use reassemble::{
    BufferLimit, PeBuffered, PeExpired, PeFlowControl, PeReassembler, PeReassemblyError,
    PeReassemblyLimits, PeReceived, PeRefused, PeStep, StreamLimit,
};
pub use value::{PeDecimal, PeExtra, PeReservedProperty, PeValue};

#[cfg(test)]
mod tests;
