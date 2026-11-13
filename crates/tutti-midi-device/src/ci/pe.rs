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
//! SysEx Message Size from Discovery. The chunks of Property Data and the
//! transaction around them are not here.
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
//! [`mcoded7`] holds the encoder and the decoder, pure functions over slices.

mod header;
mod json;
mod limits;
pub mod mcoded7;
mod value;
mod write;

pub use header::{
    PE_MAX_HEADER_LEN, PeEncoding, PeEndedBy, PeHeader, PeHeaderError, PeHeaderTooLong, PePage,
    PeReply, PeRequest, PeStatus, PeSubscription, PeUpdateCommand,
};
pub use limits::{
    MediaType, NotValid, PeMessage, PropertyName, ResourceId, ResourceName, SubscribeId,
};
pub use value::{PeDecimal, PeExtra, PeReservedProperty, PeValue};

#[cfg(test)]
mod tests;
