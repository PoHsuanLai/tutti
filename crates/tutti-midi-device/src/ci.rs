//! The MIDI-CI message codec (M2-101): the header, the MUID, and Discovery,
//! Invalidate MUID, Inquiry: Endpoint, ACK and NAK, and the Profile Configuration
//! messages; [`CiDevice`], the Management core over it; and [`ProfileInitiator`],
//! the Initiator's side of Profile Configuration.
//!
//! MIDI-CI lets two MIDI devices discover each other before either knows what the
//! other speaks. It is **not** a UMP message type: a CI message is a byte payload
//! carried inside a SysEx (sub-ID#1 `0x0D`), so it travels over MIDI 1.0 bytes
//! and over SysEx7 packets alike. This module is the codec only, over the
//! payload bytes of a SysEx message:
//!
//! ```text
//!   7E <device_id> 0D <sub_id2> <ci_ver> <src_muid:4> <dst_muid:4> <body...>
//! ```
//!
//! [`CiMessage::encode`] returns those bytes (without `F0` and `F7`); frame them
//! with `tutti_ump::frame` for a byte stream, or into SysEx7 packets, and hand
//! the bytes a `tutti_ump::SysexAssembler` completes to [`CiMessage::decode`].
//! A message the codec cannot read comes back as an [`Unread`] that keeps what
//! its header said, so the NAK that §5.3 and §5.11 require can be addressed.
//! The codec holds no state; the state machine over it is [`CiDevice`].

use std::vec::Vec;

use tutti_ump::{MidiChannel, MidiGroup, Sysex7Packets, U7, Ump};

mod device;
mod discovery;
mod endpoint;
mod initiator;
mod pe;
mod profile;
mod read;
mod status;
mod text;

pub use device::{CiAction, CiDevice, CiPeer, DISCOVERY_WINDOW};
pub use discovery::{
    CiCategories, DiscoveryData, MaxSysexSize, NotASysexSize, SUB_ID2_ACK, SUB_ID2_DISCOVERY,
    SUB_ID2_DISCOVERY_REPLY, SUB_ID2_ENDPOINT_INQUIRY, SUB_ID2_ENDPOINT_REPLY,
    SUB_ID2_INVALIDATE_MUID, SUB_ID2_NAK,
};
pub use endpoint::{CiInstanceId, EndpointInfo, EndpointStatus};
pub use initiator::{
    Outcome, PROFILE_TIMEOUT, ProfileAction, ProfileChange, ProfileEntry, ProfileInitiator,
    ProfileTable, ProfileTarget, Request, RequestError,
};
pub use pe::{
    MediaType, NotValid, PE_MAX_HEADER_LEN, PeDecimal, PeEncoding, PeEndedBy, PeExtra, PeHeader,
    PeHeaderError, PeHeaderTooLong, PeMessage, PePage, PeReply, PeRequest, PeReservedProperty,
    PeStatus, PeSubscription, PeUpdateCommand, PeValue, PropertyName, ResourceId, ResourceName,
    SubscribeId, mcoded7,
};
pub use profile::{
    ChannelsDetail, DetailsData, InquiryTarget, ListTooLong, ManufacturerProfile, ProfileChannels,
    ProfileDataError, ProfileId, ProfileLists, ProfileState, SUB_ID2_PROFILE_ADDED,
    SUB_ID2_PROFILE_DETAILS_INQUIRY, SUB_ID2_PROFILE_DETAILS_REPLY, SUB_ID2_PROFILE_DISABLED,
    SUB_ID2_PROFILE_ENABLED, SUB_ID2_PROFILE_INQUIRY, SUB_ID2_PROFILE_REMOVED,
    SUB_ID2_PROFILE_REPLY, SUB_ID2_PROFILE_SPECIFIC_DATA, SUB_ID2_SET_PROFILE_OFF,
    SUB_ID2_SET_PROFILE_ON, SpecificData, StandardProfile,
};
pub use status::{Ack, Nak};
pub use text::MessageText;

/// Universal SysEx non-real-time id, the first byte of every CI message: `0x7E`.
pub const CI_UNIVERSAL_SYSEX: u8 = 0x7E;

/// Sub-ID#1 identifying a MIDI-CI message (M2-101 §5.2.1).
pub const CI_SUB_ID_1: u8 = 0x0D;

/// The MIDI-CI Message Format Version this codec speaks: **0x02** (MIDI-CI v1.2).
///
/// M2-101 §5.3 gives version 1.2 the number 0x02, and requires a device to
/// always use its own Message Format Version, so this may only be raised alongside the message
/// bodies, which carry every Version-2 field of the messages here: Discovery's
/// Output Path ID (Table 6), the reply's Output Path Instance ID and Function
/// Block (Table 8) and NAK's details, message length and text (Table 15).
///
/// Older peers still interoperate: §5.3 requires a receiver to process only the
/// fields, values and bits of the received version when it is lower
/// than its own, and the decoders default each absent Version-2 field rather
/// than rejecting the message.
///
/// Changing the version a device sends is not a free edit (§5.3): the device
/// must invalidate its MUID and start a new Discovery Transaction.
pub const CI_VERSION: CiVersion = CiVersion::of(0x02);

/// The Message Format Version byte (M2-101 §5.2.1, §5.3, §5.4): `0x01` to
/// `0x1F`.
///
/// Version 1.1 is 0x01 and version 1.2 is 0x02. Bits 7 to 5 are reserved for
/// future major revisions, and a device that sees one set replies with a NAK
/// (error code 0x02), so a value with one set is not a version a message
/// carries; Message Format Version 0x00 is deprecated (§5.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "u8", into = "u8"))]
pub struct CiVersion(u8);

impl CiVersion {
    /// The mask of the reserved bits, 7 to 5.
    pub(crate) const RESERVED_BITS: u8 = 0xE0;

    /// The version, or `None` for `0x00` and for a value with a reserved bit
    /// set.
    #[must_use]
    pub const fn new(raw: u8) -> Option<Self> {
        if raw != 0 && raw & Self::RESERVED_BITS == 0 {
            Some(Self(raw))
        } else {
            None
        }
    }

    /// The version for a literal or a constant.
    ///
    /// # Panics
    ///
    /// For `0x00` or a value with a reserved bit set; in a `const` that is a
    /// compile error.
    #[must_use]
    #[track_caller]
    pub const fn of(raw: u8) -> Self {
        match Self::new(raw) {
            Some(version) => version,
            None => panic!("a MIDI-CI version is 0x01 to 0x1F"),
        }
    }

    /// The version byte.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

/// A raw value that is not a Message Format Version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{:#04x} is not a MIDI-CI message format version", .0)]
pub struct NotAVersion(pub u8);

impl TryFrom<u8> for CiVersion {
    type Error = NotAVersion;

    fn try_from(raw: u8) -> Result<Self, NotAVersion> {
        Self::new(raw).ok_or(NotAVersion(raw))
    }
}

impl From<CiVersion> for u8 {
    fn from(version: CiVersion) -> u8 {
        version.0
    }
}

/// The Device ID byte of a message (M2-101 §3.2 and Table 5): which Channel,
/// Group or Function Block it is to or from.
///
/// `00` to `0F` address MIDI Channels 1 to 16, `7E` the Group and `7F` the
/// Function Block; `10` to `7D` are reserved, and a reserved value is not one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "u8", into = "u8"))]
pub enum CiDeviceId {
    /// One MIDI Channel of the Group (`0x00..=0x0F`).
    Channel(MidiChannel),
    /// The whole Group, not channelized (`0x7E`).
    Group,
    /// The whole Function Block, not channelized (`0x7F`), the default of an
    /// inquiry and its reply.
    FunctionBlock,
}

impl CiDeviceId {
    /// The Device ID, or `None` for a reserved value (`0x10..=0x7D`) or one with
    /// the top bit set.
    #[must_use]
    pub const fn new(raw: u8) -> Option<Self> {
        match raw {
            0x7E => Some(Self::Group),
            0x7F => Some(Self::FunctionBlock),
            _ => match MidiChannel::new(raw) {
                Some(channel) => Some(Self::Channel(channel)),
                None => None,
            },
        }
    }

    /// The Device ID byte.
    #[must_use]
    pub const fn get(self) -> u8 {
        match self {
            Self::Channel(channel) => channel.get(),
            Self::Group => 0x7E,
            Self::FunctionBlock => 0x7F,
        }
    }
}

/// A raw value that is not a Device ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{:#04x} is not a MIDI-CI Device ID (reserved)", .0)]
pub struct NotADeviceId(pub u8);

impl TryFrom<u8> for CiDeviceId {
    type Error = NotADeviceId;

    fn try_from(raw: u8) -> Result<Self, NotADeviceId> {
        Self::new(raw).ok_or(NotADeviceId(raw))
    }
}

impl From<CiDeviceId> for u8 {
    fn from(id: CiDeviceId) -> u8 {
        id.get()
    }
}

/// A 28-bit **MIDI Unique Identifier** (M2-101 §3.3): the random address a CI
/// device gives itself. Four 7-bit bytes, least significant first, on the wire.
///
/// It holds a MUID a device can *own*: the reserved range
/// `RESERVED_START` to `0x0FFF_FFFE` and the broadcast
/// value `BROADCAST_RAW` are refused. The broadcast
/// address is a [`Destination`], not a `Muid`. A MUID is drawn from an OS random
/// source by whoever owns the device; this type only checks one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Muid(u32);

impl Muid {
    /// The first reserved MUID, `0x0FFF_FF00` (M2-101 §3.3: `0x0FFF_FF00` to
    /// `0x0FFF_FFFE` are reserved).
    pub(crate) const RESERVED_START: u32 = 0x0FFF_FF00;
    /// The broadcast value, `0x0FFF_FFFF` (M2-101 §3.3.3).
    pub(crate) const BROADCAST_RAW: u32 = 0x0FFF_FFFF;

    /// A MUID, or `None` for a value past 28 bits, in the reserved range or the
    /// broadcast value.
    #[must_use]
    pub const fn new(raw: u32) -> Option<Self> {
        if raw < Self::RESERVED_START {
            Some(Muid(raw))
        } else {
            None
        }
    }

    /// A MUID from a caller's random word: its low 28 bits, or `None` when
    /// they fall in the reserved range or on the broadcast value, so the caller
    /// draws again. No random source is named here: a MUID should come from a
    /// good one (M2-101 Appendix A.1), and whose is the owner's.
    #[must_use]
    pub const fn from_entropy(word: u32) -> Option<Self> {
        Self::new(word & Self::BROADCAST_RAW)
    }

    /// The 28-bit value.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// The four little-endian 7-bit wire bytes.
    #[must_use]
    pub fn to_bytes(self) -> [u8; 4] {
        wire_bytes(self.0)
    }

    /// The MUID of four little-endian 7-bit wire bytes, or `None` for a byte
    /// with its top bit set, a reserved value or the broadcast value.
    #[must_use]
    pub fn from_bytes(b: [u8; 4]) -> Option<Self> {
        Self::new(from_wire_bytes(b)?)
    }
}

/// A raw value that is not a MUID a device can own ([`Muid::new`] refused it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{:#010x} is not a MUID (past 28 bits, reserved, or broadcast)", .0)]
pub struct NotAMuid(pub u32);

impl TryFrom<u32> for Muid {
    type Error = NotAMuid;

    fn try_from(raw: u32) -> Result<Self, NotAMuid> {
        Self::new(raw).ok_or(NotAMuid(raw))
    }
}

impl From<Muid> for u32 {
    fn from(muid: Muid) -> u32 {
        muid.0
    }
}

/// Who a CI message is for: one device, or all of them (M2-101 §3.3.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Destination {
    /// The broadcast MUID, `0x0FFF_FFFF`.
    Broadcast,
    /// One device.
    Muid(Muid),
}

impl Destination {
    fn to_bytes(self) -> [u8; 4] {
        match self {
            Destination::Broadcast => wire_bytes(Muid::BROADCAST_RAW),
            Destination::Muid(muid) => muid.to_bytes(),
        }
    }

    fn from_bytes(b: [u8; 4]) -> Option<Self> {
        let raw = from_wire_bytes(b)?;
        if raw == Muid::BROADCAST_RAW {
            Some(Destination::Broadcast)
        } else {
            Muid::new(raw).map(Destination::Muid)
        }
    }
}

/// A 14-bit value as two 7-bit bytes, least significant first.
fn wire_u14(raw: u16) -> [u8; 2] {
    [(raw & 0x7F) as u8, ((raw >> 7) & 0x7F) as u8]
}

/// A 28-bit value as four little-endian 7-bit bytes.
fn wire_bytes(raw: u32) -> [u8; 4] {
    [
        (raw & 0x7F) as u8,
        ((raw >> 7) & 0x7F) as u8,
        ((raw >> 14) & 0x7F) as u8,
        ((raw >> 21) & 0x7F) as u8,
    ]
}

/// Four little-endian 7-bit bytes as a 28-bit value, or `None` for a byte with
/// its top bit set.
fn from_wire_bytes(b: [u8; 4]) -> Option<u32> {
    if b.iter().any(|byte| byte & 0x80 != 0) {
        return None;
    }
    Some(
        u32::from(b[0])
            | (u32::from(b[1]) << 7)
            | (u32::from(b[2]) << 14)
            | (u32::from(b[3]) << 21),
    )
}

/// The fixed MIDI-CI preamble shared by every message (M2-101 §3.3 and §5.2.1):
/// the Device ID, the CI version and the sender's MUID. The sub-ID#2 (message
/// type) is **not** stored here: the [`CiMessage`] variant determines it, and so
/// it determines the destination MUID too, which is why neither is a field.
///
/// M2-101 §3.3.3 limits the Broadcast MUID to Discovery, Invalidate MUID,
/// the Profile Added, Removed, Enabled and Disabled Reports, and Profile
/// Specific Data. Discovery and Invalidate MUID are defined with the Broadcast
/// MUID as destination (Tables 6 and 12); Discovery Reply, ACK and NAK are not
/// on the list, so they go to one device. A [`CiMessage`] holds exactly that
/// and no other: a variant that is broadcast has no destination field, and one
/// that is not has a [`Muid`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CiHeader {
    /// The Device ID: a Channel, the Group, or the Function Block.
    pub device_id: CiDeviceId,
    /// CI message version (normally [`CI_VERSION`]).
    pub ci_version: CiVersion,
    /// MUID of the sender.
    pub source: Muid,
}

impl CiHeader {
    /// A header from `source`, to the whole Function Block, at [`CI_VERSION`]:
    /// the default of an inquiry and its reply (§5.2.1).
    #[must_use]
    pub const fn new(source: Muid) -> Self {
        Self {
            device_id: CiDeviceId::FunctionBlock,
            ci_version: CI_VERSION,
            source,
        }
    }

    /// Append the preamble bytes to `out`, stamping the given `sub_id2` and
    /// `destination`.
    fn encode(&self, sub_id2: u8, destination: Destination, out: &mut impl Extend<u8>) {
        out.extend([
            CI_UNIVERSAL_SYSEX,
            self.device_id.get(),
            CI_SUB_ID_1,
            sub_id2,
            self.ci_version.get(),
        ]);
        out.extend(self.source.to_bytes());
        out.extend(destination.to_bytes());
    }
}

/// A decoded MIDI-CI message: its [`CiHeader`] plus a typed body. Non-exhaustive:
/// a message kind this codec does not model decodes to an [`Unread`] with
/// [`UnreadReason::UnsupportedMessage`], and one may be added.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CiMessage {
    /// Discovery, the opening handshake advertising identity and capabilities
    /// (M2-101 §5.5). Always to the Broadcast MUID.
    Discovery {
        /// Addressing preamble.
        header: CiHeader,
        /// Advertised identity and capabilities. Its Function Block is not on
        /// the wire of a Discovery and is ignored.
        data: DiscoveryData,
    },
    /// Reply to Discovery (M2-101 §5.6): the same body, to the initiator.
    DiscoveryReply {
        /// Addressing preamble.
        header: CiHeader,
        /// The initiator, whose MUID the Discovery came from.
        to: Muid,
        /// Advertised identity and capabilities.
        data: DiscoveryData,
    },
    /// Invalidate MUID: tells a device its MUID collided and it must pick a new
    /// one (M2-101 §5.9). Always to the Broadcast MUID. Carries the target MUID.
    InvalidateMuid {
        /// Addressing preamble.
        header: CiHeader,
        /// The MUID being invalidated: the colliding one, not the sender's.
        target: Muid,
    },
    /// Inquiry: Endpoint (M2-101 §5.7): asks the UMP Endpoint behind a Function
    /// Block for one piece of information. To one device, never broadcast.
    EndpointInquiry {
        /// Addressing preamble.
        header: CiHeader,
        /// The device asked.
        to: Muid,
        /// What is asked.
        status: EndpointStatus,
    },
    /// Reply to Endpoint (M2-101 §5.8): the Status and the data it names.
    EndpointReply {
        /// Addressing preamble.
        header: CiHeader,
        /// The device that asked.
        to: Muid,
        /// The answer.
        info: EndpointInfo,
    },
    /// ACK: a positive answer or a notice of activity under way (M2-101 §5.10).
    Ack {
        /// Addressing preamble.
        header: CiHeader,
        /// The device being answered.
        to: Muid,
        /// What it says.
        ack: Ack,
    },
    /// NAK: a device rejects a CI message it cannot handle (M2-101 §5.11).
    Nak {
        /// Addressing preamble.
        header: CiHeader,
        /// The device being answered.
        to: Muid,
        /// Which message was rejected, and why.
        nak: Nak,
    },
    /// Profile Inquiry (M2-101 §7.2): asks which Profiles a device supports at
    /// the address in the header. To one device.
    ProfileInquiry {
        /// Addressing preamble; its Device ID is the address asked.
        header: CiHeader,
        /// The device asked.
        to: Muid,
    },
    /// Reply to Profile Inquiry (§7.3): the Profiles enabled and disabled at the
    /// address in the header.
    ProfileReply {
        /// Addressing preamble; its Device ID is the address described.
        header: CiHeader,
        /// The device that asked.
        to: Muid,
        /// The two lists.
        profiles: ProfileLists,
    },
    /// Set Profile On (§7.8): enables a Profile at the address in the header.
    SetProfileOn {
        /// Addressing preamble; its Device ID is the address.
        header: CiHeader,
        /// The device asked.
        to: Muid,
        /// The Profile to enable.
        profile: ProfileId,
        /// `Number Channels Requested`: 0 for a Group or Function Block address.
        /// A Version 1 sender has no such field and it reads as 0.
        channels: ProfileChannels,
    },
    /// Set Profile Off (§7.9): disables a Profile at the address in the header.
    SetProfileOff {
        /// Addressing preamble; its Device ID is the address.
        header: CiHeader,
        /// The device asked.
        to: Muid,
        /// The Profile to disable.
        profile: ProfileId,
    },
    /// Profile Enabled Report (§7.10): a Profile is now enabled. Always sent to the
    /// Broadcast MUID.
    ProfileEnabled {
        /// Addressing preamble; its Device ID is the address.
        header: CiHeader,
        /// The Profile.
        profile: ProfileId,
        /// `Number Channels enabled`: 0 for a Group or Function Block address,
        /// and for a Version 1 sender, which has no such field.
        channels: ProfileChannels,
    },
    /// Profile Disabled Report (§7.11): a Profile is now disabled. Always sent to the
    /// Broadcast MUID.
    ProfileDisabled {
        /// Addressing preamble; its Device ID is the address.
        header: CiHeader,
        /// The Profile.
        profile: ProfileId,
        /// `Number Channels disabled`: as [`ProfileEnabled`](Self::ProfileEnabled).
        channels: ProfileChannels,
    },
    /// Profile Added Report (§7.4): a Profile is newly supported. To the
    /// Broadcast MUID.
    ProfileAdded {
        /// Addressing preamble; its Device ID is the address.
        header: CiHeader,
        /// The Profile.
        profile: ProfileId,
    },
    /// Profile Removed Report (§7.5): a Profile is no longer supported. To the
    /// Broadcast MUID.
    ProfileRemoved {
        /// Addressing preamble; its Device ID is the address.
        header: CiHeader,
        /// The Profile.
        profile: ProfileId,
    },
    /// Profile Details Inquiry (§7.6): asks for a detail of a Profile's
    /// implementation.
    ProfileDetailsInquiry {
        /// Addressing preamble; its Device ID is the address.
        header: CiHeader,
        /// The device asked.
        to: Muid,
        /// The Profile.
        profile: ProfileId,
        /// What is asked.
        target: InquiryTarget,
    },
    /// Reply to Profile Details Inquiry (§7.7).
    ProfileDetailsReply {
        /// Addressing preamble; its Device ID is the address.
        header: CiHeader,
        /// The device that asked.
        to: Muid,
        /// The Profile.
        profile: ProfileId,
        /// The target answered, as it was in the Profile Details Inquiry.
        target: InquiryTarget,
        /// The data of that target.
        data: DetailsData,
    },
    /// Profile Specific Data (§7.12): data the Profile's specification defines.
    /// To one device or, when the Profile says so, to the Broadcast MUID.
    ProfileSpecificData {
        /// Addressing preamble; its Device ID is the address.
        header: CiHeader,
        /// To one device or to all.
        to: Destination,
        /// The Profile.
        profile: ProfileId,
        /// The data.
        data: SpecificData,
    },
}

impl CiMessage {
    /// The message's preamble.
    #[must_use]
    pub fn header(&self) -> &CiHeader {
        match self {
            CiMessage::Discovery { header, .. }
            | CiMessage::DiscoveryReply { header, .. }
            | CiMessage::InvalidateMuid { header, .. }
            | CiMessage::EndpointInquiry { header, .. }
            | CiMessage::EndpointReply { header, .. }
            | CiMessage::Ack { header, .. }
            | CiMessage::Nak { header, .. }
            | CiMessage::ProfileInquiry { header, .. }
            | CiMessage::ProfileReply { header, .. }
            | CiMessage::SetProfileOn { header, .. }
            | CiMessage::SetProfileOff { header, .. }
            | CiMessage::ProfileEnabled { header, .. }
            | CiMessage::ProfileDisabled { header, .. }
            | CiMessage::ProfileAdded { header, .. }
            | CiMessage::ProfileRemoved { header, .. }
            | CiMessage::ProfileDetailsInquiry { header, .. }
            | CiMessage::ProfileDetailsReply { header, .. }
            | CiMessage::ProfileSpecificData { header, .. } => header,
        }
    }

    /// The Destination MUID: the Broadcast MUID for Discovery, Invalidate MUID and
    /// the Profile reports, the one device for the rest (Profile Specific Data
    /// either).
    #[must_use]
    pub fn destination(&self) -> Destination {
        match self {
            CiMessage::Discovery { .. }
            | CiMessage::InvalidateMuid { .. }
            | CiMessage::ProfileEnabled { .. }
            | CiMessage::ProfileDisabled { .. }
            | CiMessage::ProfileAdded { .. }
            | CiMessage::ProfileRemoved { .. } => Destination::Broadcast,
            CiMessage::ProfileSpecificData { to, .. } => *to,
            CiMessage::DiscoveryReply { to, .. }
            | CiMessage::EndpointInquiry { to, .. }
            | CiMessage::EndpointReply { to, .. }
            | CiMessage::Ack { to, .. }
            | CiMessage::Nak { to, .. }
            | CiMessage::ProfileInquiry { to, .. }
            | CiMessage::ProfileReply { to, .. }
            | CiMessage::SetProfileOn { to, .. }
            | CiMessage::SetProfileOff { to, .. }
            | CiMessage::ProfileDetailsInquiry { to, .. }
            | CiMessage::ProfileDetailsReply { to, .. } => Destination::Muid(*to),
        }
    }

    /// The message's sub-ID#2.
    #[must_use]
    pub fn sub_id2(&self) -> u8 {
        match self {
            CiMessage::Discovery { .. } => SUB_ID2_DISCOVERY,
            CiMessage::DiscoveryReply { .. } => SUB_ID2_DISCOVERY_REPLY,
            CiMessage::InvalidateMuid { .. } => SUB_ID2_INVALIDATE_MUID,
            CiMessage::EndpointInquiry { .. } => SUB_ID2_ENDPOINT_INQUIRY,
            CiMessage::EndpointReply { .. } => SUB_ID2_ENDPOINT_REPLY,
            CiMessage::Ack { .. } => SUB_ID2_ACK,
            CiMessage::Nak { .. } => SUB_ID2_NAK,
            CiMessage::ProfileInquiry { .. } => SUB_ID2_PROFILE_INQUIRY,
            CiMessage::ProfileReply { .. } => SUB_ID2_PROFILE_REPLY,
            CiMessage::SetProfileOn { .. } => SUB_ID2_SET_PROFILE_ON,
            CiMessage::SetProfileOff { .. } => SUB_ID2_SET_PROFILE_OFF,
            CiMessage::ProfileEnabled { .. } => SUB_ID2_PROFILE_ENABLED,
            CiMessage::ProfileDisabled { .. } => SUB_ID2_PROFILE_DISABLED,
            CiMessage::ProfileAdded { .. } => SUB_ID2_PROFILE_ADDED,
            CiMessage::ProfileRemoved { .. } => SUB_ID2_PROFILE_REMOVED,
            CiMessage::ProfileDetailsInquiry { .. } => SUB_ID2_PROFILE_DETAILS_INQUIRY,
            CiMessage::ProfileDetailsReply { .. } => SUB_ID2_PROFILE_DETAILS_REPLY,
            CiMessage::ProfileSpecificData { .. } => SUB_ID2_PROFILE_SPECIFIC_DATA,
        }
    }

    /// The CI byte payload: the bytes carried *inside* a SysEx, starting at
    /// `0x7E`, without the `0xF0` and `0xF7` delimiters.
    ///
    /// [`encode_into`](Self::encode_into) writes them into a buffer of the
    /// caller's, and [`packets`](Self::packets) cuts them into UMP packets.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    /// Appends the CI byte payload (what [`encode`](Self::encode) returns) to
    /// `out`: a `Vec<u8>`, a `VecDeque`, any collection of bytes.
    pub fn encode_into(&self, out: &mut impl Extend<u8>) {
        self.header()
            .encode(self.sub_id2(), self.destination(), out);
        match self {
            CiMessage::Discovery { data, .. } => data.encode_body(false, out),
            CiMessage::DiscoveryReply { data, .. } => data.encode_body(true, out),
            CiMessage::InvalidateMuid { target, .. } => out.extend(target.to_bytes()),
            CiMessage::EndpointInquiry { status, .. } => out.extend([status.get().get()]),
            CiMessage::EndpointReply { info, .. } => info.encode_body(out),
            CiMessage::Ack { ack, .. } => ack.encode_body(out),
            CiMessage::Nak { nak, .. } => nak.encode_body(out),
            CiMessage::ProfileInquiry { .. } => {}
            CiMessage::ProfileReply { profiles, .. } => profiles.encode_body(out),
            CiMessage::SetProfileOn {
                profile, channels, ..
            } => {
                out.extend(profile.to_bytes());
                out.extend(wire_u14(channels.get()));
            }
            // Two reserved bytes (Table 25).
            CiMessage::SetProfileOff { profile, .. } => {
                out.extend(profile.to_bytes());
                out.extend([0, 0]);
            }
            CiMessage::ProfileEnabled {
                profile, channels, ..
            }
            | CiMessage::ProfileDisabled {
                profile, channels, ..
            } => {
                out.extend(profile.to_bytes());
                out.extend(wire_u14(channels.get()));
            }
            CiMessage::ProfileAdded { profile, .. } | CiMessage::ProfileRemoved { profile, .. } => {
                out.extend(profile.to_bytes());
            }
            CiMessage::ProfileDetailsInquiry {
                profile, target, ..
            } => {
                out.extend(profile.to_bytes());
                out.extend([target.get().get()]);
            }
            CiMessage::ProfileDetailsReply {
                profile,
                target,
                data,
                ..
            } => {
                out.extend(profile.to_bytes());
                out.extend([target.get().get()]);
                // Table 23: 14-bit length, least significant byte first.
                out.extend(wire_u14(data.as_bytes().len() as u16));
                out.extend(data.as_bytes().iter().copied());
            }
            CiMessage::ProfileSpecificData { profile, data, .. } => {
                out.extend(profile.to_bytes());
                // Table 28: a 4-byte length of the Profile Specific Data that follows, LSB first.
                out.extend(wire_bytes(data.as_bytes().len() as u32));
                out.extend(data.as_bytes().iter().copied());
            }
        }
    }

    /// The message as SysEx7 packets on `group` (M2-104 section 7.7), ready to
    /// send on a UMP stream: the payload of [`encode`](Self::encode) cut into a
    /// `Complete`, or a `Start`, `Continue`s and an `End`. It allocates once,
    /// for the payload; a MIDI 1.0 byte stream frames
    /// [`encode`](Self::encode)'s bytes with `tutti_ump::frame` instead.
    pub fn packets(&self, group: MidiGroup) -> std::vec::IntoIter<Ump> {
        let payload = self.encode();
        // Every byte a CI message encodes is 7-bit: the fields are `U7`, the
        // MUIDs and sizes are cut to 7-bit bytes and a text is ASCII.
        Sysex7Packets::from_payload(group, &payload)
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .into_iter()
    }

    /// Decodes a CI byte payload (as [`encode`](Self::encode) produces, or a
    /// `tutti_ump::SysexAssembler` completed) into a typed message, or into
    /// what could be read of one that is not ([`Decoded`]).
    pub fn decode(bytes: &[u8]) -> Decoded {
        // Not a CI message at all, or too short to hold the preamble: there is
        // no header to keep, and no sender a NAK could be addressed to.
        let Some((preamble, body)) = read::preamble(bytes) else {
            return Decoded::NotCi;
        };
        let read::Preamble {
            device_id,
            sub_id2,
            version,
            source,
            destination,
        } = preamble;
        let unread = |reason| {
            Decoded::Unread(Unread {
                device_id,
                sub_id2,
                version,
                source,
                destination,
                reason,
            })
        };

        // The header first, in the order a reply would have to take it: §5.3's
        // reserved version bits, §5.4's version 0, Table 5's reserved Device IDs.
        let Some(ci_version) = CiVersion::new(version) else {
            return unread(if version == 0 {
                UnreadReason::VersionZero
            } else {
                UnreadReason::ReservedVersionBits
            });
        };
        let Some(device_id) = CiDeviceId::new(device_id) else {
            return unread(UnreadReason::ReservedDeviceId);
        };
        let Some(source) = source else {
            return unread(UnreadReason::BadSource);
        };
        let Some(destination) = destination else {
            return unread(UnreadReason::BadDestination);
        };
        let header = CiHeader {
            device_id,
            ci_version,
            source,
        };

        // The destination each message is defined with (§3.3.3): `Some(to)` is
        // the one device, `None` the Broadcast MUID.
        let unicast = match destination {
            Destination::Muid(muid) => Some(muid),
            Destination::Broadcast => None,
        };
        let message =
            match (sub_id2, unicast) {
                (SUB_ID2_DISCOVERY, None) => DiscoveryData::decode_body(false, body)
                    .map(|data| CiMessage::Discovery { header, data }),
                (SUB_ID2_INVALIDATE_MUID, None) => read::target_body(body)
                    .map(|target| CiMessage::InvalidateMuid { header, target }),
                (SUB_ID2_DISCOVERY_REPLY, Some(to)) => DiscoveryData::decode_body(true, body)
                    .map(|data| CiMessage::DiscoveryReply { header, to, data }),
                (SUB_ID2_ENDPOINT_INQUIRY, Some(to)) => match read::endpoint_inquiry_body(body) {
                    None => None,
                    Some(raw) => match EndpointStatus::new(raw) {
                        Some(status) => Some(CiMessage::EndpointInquiry { header, to, status }),
                        None => return unread(UnreadReason::ReservedStatus),
                    },
                },
                (SUB_ID2_ENDPOINT_REPLY, Some(to)) => match read::endpoint_reply_body(body) {
                    None => None,
                    Some((raw, data)) => match EndpointStatus::new(raw) {
                        Some(EndpointStatus::ProductInstanceId) => CiInstanceId::from_bytes(data)
                            .map(|id| CiMessage::EndpointReply {
                                header,
                                to,
                                info: EndpointInfo::ProductInstanceId(id),
                            }),
                        None => return unread(UnreadReason::ReservedStatus),
                    },
                },
                (SUB_ID2_PROFILE_INQUIRY, Some(to)) => {
                    Some(CiMessage::ProfileInquiry { header, to })
                }
                (SUB_ID2_PROFILE_REPLY, Some(to)) => {
                    read::profile_reply_body(body).map(|profiles| CiMessage::ProfileReply {
                        header,
                        to,
                        profiles,
                    })
                }
                (SUB_ID2_SET_PROFILE_ON, Some(to)) => {
                    read::profile_with_count_body(body).map(|(profile, channels)| {
                        CiMessage::SetProfileOn {
                            header,
                            to,
                            profile,
                            channels,
                        }
                    })
                }
                (SUB_ID2_SET_PROFILE_OFF, Some(to)) => {
                    read::profile_body(body).map(|profile| CiMessage::SetProfileOff {
                        header,
                        to,
                        profile,
                    })
                }
                (SUB_ID2_PROFILE_ENABLED, None) => {
                    read::profile_with_count_body(body).map(|(profile, channels)| {
                        CiMessage::ProfileEnabled {
                            header,
                            profile,
                            channels,
                        }
                    })
                }
                (SUB_ID2_PROFILE_DISABLED, None) => {
                    read::profile_with_count_body(body).map(|(profile, channels)| {
                        CiMessage::ProfileDisabled {
                            header,
                            profile,
                            channels,
                        }
                    })
                }
                (SUB_ID2_PROFILE_ADDED, None) => read::profile_body(body)
                    .map(|profile| CiMessage::ProfileAdded { header, profile }),
                (SUB_ID2_PROFILE_REMOVED, None) => read::profile_body(body)
                    .map(|profile| CiMessage::ProfileRemoved { header, profile }),
                (SUB_ID2_PROFILE_DETAILS_INQUIRY, Some(to)) => read::details_inquiry_body(body)
                    .map(|(profile, target)| CiMessage::ProfileDetailsInquiry {
                        header,
                        to,
                        profile,
                        target,
                    }),
                (SUB_ID2_PROFILE_DETAILS_REPLY, Some(to)) => {
                    read::details_reply_body(body).map(|(profile, target, data)| {
                        CiMessage::ProfileDetailsReply {
                            header,
                            to,
                            profile,
                            target,
                            data,
                        }
                    })
                }
                (SUB_ID2_PROFILE_SPECIFIC_DATA, _) => {
                    read::specific_data_body(body).map(|(profile, data)| {
                        CiMessage::ProfileSpecificData {
                            header,
                            to: destination,
                            profile,
                            data,
                        }
                    })
                }
                (SUB_ID2_ACK, Some(to)) => {
                    Ack::decode_body(body).map(|ack| CiMessage::Ack { header, to, ack })
                }
                (SUB_ID2_NAK, Some(to)) => {
                    Nak::decode_body(body).map(|nak| CiMessage::Nak { header, to, nak })
                }
                // A message sent to the one kind of destination it is not defined
                // with: Broadcast where it must not be, a device where it must be
                // Broadcast.
                (
                    SUB_ID2_DISCOVERY
                    | SUB_ID2_INVALIDATE_MUID
                    | SUB_ID2_DISCOVERY_REPLY
                    | SUB_ID2_ENDPOINT_INQUIRY
                    | SUB_ID2_ENDPOINT_REPLY
                    | SUB_ID2_ACK
                    | SUB_ID2_NAK
                    | SUB_ID2_PROFILE_INQUIRY
                    | SUB_ID2_PROFILE_REPLY
                    | SUB_ID2_SET_PROFILE_ON
                    | SUB_ID2_SET_PROFILE_OFF
                    | SUB_ID2_PROFILE_ENABLED
                    | SUB_ID2_PROFILE_DISABLED
                    | SUB_ID2_PROFILE_ADDED
                    | SUB_ID2_PROFILE_REMOVED
                    | SUB_ID2_PROFILE_DETAILS_INQUIRY
                    | SUB_ID2_PROFILE_DETAILS_REPLY,
                    _,
                ) => return unread(UnreadReason::WrongDestination),
                _ => return unread(UnreadReason::UnsupportedMessage),
            };
        match message {
            Some(message) => Decoded::Message(message),
            None => unread(UnreadReason::Malformed),
        }
    }
}

/// What [`CiMessage::decode`] made of a payload.
#[derive(Clone, Debug, PartialEq, Eq)]
#[must_use]
pub enum Decoded {
    /// A message this codec models.
    Message(CiMessage),
    /// A MIDI-CI message that could not be read, with what its header said.
    Unread(Unread),
    /// Not a MIDI-CI message (not `7E <id> 0D`), or too short to hold the 13-byte
    /// preamble.
    NotCi,
}

/// A MIDI-CI message the codec could not read, with the fields of its preamble
/// that were there: what a device needs to address the NAK that §5.3 and §5.11
/// require: a reply to a message the device does not support, one of a version
/// or format it does not support, a malformed one, or one whose Message Format
/// Version has reserved bits set.
///
/// The fields are the raw bytes, because the reason may be that one of them was
/// not valid: the Device ID may be reserved, the version may be 0, the source may
/// be a MUID nobody can own (`None`, and then there is nobody to answer).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unread {
    /// The Device ID byte as sent.
    pub device_id: u8,
    /// The sub-ID#2 byte as sent.
    pub sub_id2: u8,
    /// The Message Format Version byte as sent.
    pub version: u8,
    /// The sender's MUID, or `None` when the four bytes are not one a device can
    /// own.
    pub source: Option<Muid>,
    /// The Destination MUID, or `None` when it is not a MUID or the Broadcast
    /// MUID.
    pub destination: Option<Destination>,
    /// Why it was not read.
    pub reason: UnreadReason,
}

/// Why a MIDI-CI message was not read ([`Unread::reason`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum UnreadReason {
    /// A reserved bit (7 to 5) of the Message Format Version is set (§5.3).
    ReservedVersionBits,
    /// Message Format Version `0x00`, which is deprecated (§5.4).
    VersionZero,
    /// The Device ID is in the reserved range `0x10..=0x7D` (Table 5).
    ReservedDeviceId,
    /// The source MUID is reserved or the Broadcast MUID: a device cannot own
    /// either (§3.3).
    BadSource,
    /// The destination MUID is reserved (§3.3).
    BadDestination,
    /// A sub-ID#2 this codec does not model.
    UnsupportedMessage,
    /// An Inquiry: Endpoint or its reply with a Status that is reserved
    /// (`0x01..=0x7F`, Table 10): the Device cannot give that information.
    ReservedStatus,
    /// A message of a kind it models, sent to the wrong kind of destination
    /// (§3.3.3): Discovery, Invalidate MUID or a Profile report to one device, or a
    /// reply, an Endpoint message, a Profile inquiry or Set, ACK or NAK to the
    /// Broadcast MUID.
    WrongDestination,
    /// The body is malformed: too short, a byte with its top bit set, or a field
    /// out of range (a Receivable Maximum SysEx Message Size under 128, a
    /// Function Block that is not 0 to 31 or `0x7F`, a text that runs past the
    /// message).
    Malformed,
}

impl UnreadReason {
    /// The NAK Status Code Table 16 gives this: `0x02` (version not
    /// supported) for a version the Device cannot read (§5.3 names reserved
    /// bits; version 0 is tutti's reading), `0x01` (message not
    /// supported), else `0x41` (message malformed).
    #[must_use]
    pub(crate) fn nak_status(self) -> U7 {
        match self {
            Self::ReservedVersionBits | Self::VersionZero => Nak::STATUS_VERSION_NOT_SUPPORTED,
            Self::UnsupportedMessage => Nak::STATUS_MESSAGE_NOT_SUPPORTED,
            // §5.8: a device that cannot provide the information for the given
            // Status replies with a NAK: the generic one, which is not retried.
            Self::ReservedStatus => Nak::STATUS_NAK,
            _ => Nak::STATUS_MALFORMED,
        }
    }
}

impl Unread {
    /// The NAK that answers this message from the device `own`: to the sender,
    /// on the Device ID it used (the Function Block when that was reserved), in
    /// this codec's [`CI_VERSION`] (the Receiver's own Message Format
    /// Version, §5.4), with the Status Code of `UnreadReason::nak_status` and
    /// the failed sub-ID#2 as the Original Transaction.
    ///
    /// `None` when there is nobody to answer (the source was not a MUID), when
    /// the sub-ID#2 is not seven bits, and for an ACK or a NAK, which are never
    /// answered with a NAK: a pair of devices would trade them forever (the spec
    /// does not say; this is tutti's rule).
    #[must_use]
    pub fn nak(&self, own: Muid) -> Option<CiMessage> {
        if matches!(self.sub_id2, SUB_ID2_ACK | SUB_ID2_NAK) {
            return None;
        }
        Some(CiMessage::Nak {
            header: CiHeader {
                device_id: CiDeviceId::new(self.device_id).unwrap_or(CiDeviceId::FunctionBlock),
                ci_version: CI_VERSION,
                source: own,
            },
            to: self.source?,
            nak: Nak::new(U7::new(self.sub_id2)?, self.reason.nak_status()),
        })
    }
}

#[cfg(test)]
mod tests;
