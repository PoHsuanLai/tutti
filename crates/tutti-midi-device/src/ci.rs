//! The MIDI-CI message codec (M2-101): the header, the MUID, and the Discovery,
//! Invalidate MUID and NAK messages.
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
//! There is no state machine and no inbound path here (design 017 §5, "MIDI-CI"
//! lists what a rebuild needs).

use std::fmt;
use std::vec::Vec;

mod discovery;

pub use discovery::{
    CiCategories, DiscoveryData, NO_FUNCTION_BLOCK, Nak, SUB_ID2_DISCOVERY,
    SUB_ID2_DISCOVERY_REPLY, SUB_ID2_INVALIDATE_MUID, SUB_ID2_NAK,
};

/// Universal SysEx non-real-time id, the first byte of every CI message: `0x7E`.
pub const CI_UNIVERSAL_SYSEX: u8 = 0x7E;

/// Sub-ID#1 identifying a MIDI-CI message (M2-101 §5.2.1).
pub const CI_SUB_ID_1: u8 = 0x0D;

/// The MIDI-CI Message Format Version this codec speaks: **0x02** (MIDI-CI v1.2).
///
/// M2-101 §5.3: "In this version 1.2 of the MIDI-CI Specification, the version
/// number is 0x02." The same section requires a device to "always use its own
/// Message Format Version", so this may only be raised alongside the message
/// bodies, which carry every Version-2 field of the messages here: Discovery's
/// Output Path ID (Table 6), the reply's Output Path Instance ID and Function
/// Block (Table 8) and NAK's details, message length and text (Table 15).
///
/// Older peers still interoperate: §5.3 requires a receiver to "process the
/// fields, values, and bits defined in the received version" when it is lower
/// than its own, and the decoders default each absent Version-2 field rather
/// than rejecting the message.
///
/// Changing the version a device sends is not a free edit (§5.3): it "shall
/// invalidate its MUID and initiate a new Discovery Transaction".
pub const CI_VERSION: u8 = 0x02;

/// The device-id byte for a message to the whole Function Block or port: `0x7F`
/// (M2-101 §3.3.3).
pub const CI_DEVICE_ID_FUNCTION_BLOCK: u8 = 0x7F;

/// A 28-bit **MIDI Unique Identifier** (M2-101 §3.3): the random address a CI
/// device gives itself. Four 7-bit bytes, least significant first, on the wire.
///
/// It holds a MUID a device can *own*: the reserved range
/// [`RESERVED_START`](Self::RESERVED_START) to `0x0FFF_FFFE` and the broadcast
/// value [`BROADCAST_RAW`](Self::BROADCAST_RAW) are refused. The broadcast
/// address is a [`Destination`], not a `Muid`. A MUID is drawn from an OS random
/// source by whoever owns the device; this type only checks one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Muid(u32);

impl Muid {
    /// The first reserved MUID, `0x0FFF_FF00` (M2-101 §3.3: `0x0FFF_FF00` to
    /// `0x0FFF_FFFE` are reserved).
    pub const RESERVED_START: u32 = 0x0FFF_FF00;
    /// The broadcast value, `0x0FFF_FFFF` (M2-101 §3.3.3).
    pub const BROADCAST_RAW: u32 = 0x0FFF_FFFF;

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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotAMuid(pub u32);

impl fmt::Display for NotAMuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:#010x} is not a MUID (past 28 bits, reserved, or broadcast)",
            self.0
        )
    }
}

impl std::error::Error for NotAMuid {}

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
/// the source and destination MUIDs, the CI version, and the device-id byte that
/// scopes the message. The sub-ID#2 (message type) is **not** stored here: the
/// [`CiMessage`] variant determines it, so it is derived on encode and used to
/// dispatch on decode rather than duplicated in the header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CiHeader {
    /// Device-id byte: a channel `0x00..=0x0F`, or [`CI_DEVICE_ID_FUNCTION_BLOCK`].
    pub device_id: u8,
    /// CI message version (normally [`CI_VERSION`]).
    pub ci_version: u8,
    /// MUID of the sender.
    pub source: Muid,
    /// The intended recipient, or every device on the port.
    pub destination: Destination,
}

impl CiHeader {
    /// Length of the encoded preamble in bytes: `7E id 0D sub2 ver` + 2x4 MUID.
    pub const LEN: usize = 5 + 4 + 4;

    /// Append the preamble bytes to `out`, stamping the given `sub_id2`.
    fn encode(&self, sub_id2: u8, out: &mut Vec<u8>) {
        out.push(CI_UNIVERSAL_SYSEX);
        out.push(self.device_id);
        out.push(CI_SUB_ID_1);
        out.push(sub_id2);
        out.push(self.ci_version);
        out.extend_from_slice(&self.source.to_bytes());
        out.extend_from_slice(&self.destination.to_bytes());
    }

    /// Parse the preamble from the front of `bytes`, returning the header, its
    /// sub-ID#2, and the offset where the body begins; `None` if `bytes` is not
    /// a CI message or carries a MUID no device can own as its source.
    fn decode(bytes: &[u8]) -> Option<(CiHeader, u8, usize)> {
        if bytes.len() < Self::LEN {
            return None;
        }
        if bytes[0] != CI_UNIVERSAL_SYSEX || bytes[2] != CI_SUB_ID_1 {
            return None;
        }
        let header = CiHeader {
            device_id: bytes[1],
            ci_version: bytes[4],
            source: Muid::from_bytes([bytes[5], bytes[6], bytes[7], bytes[8]])?,
            destination: Destination::from_bytes([bytes[9], bytes[10], bytes[11], bytes[12]])?,
        };
        Some((header, bytes[3], Self::LEN))
    }
}

/// A decoded MIDI-CI message: its [`CiHeader`] plus a typed body. Non-exhaustive:
/// a message kind this codec does not model decodes to `None`, and one may be
/// added.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CiMessage {
    /// Discovery or Discovery Reply, the opening handshake advertising identity
    /// and capabilities (M2-101 §5.5). `is_reply` tells the two apart.
    Discovery {
        /// Addressing preamble.
        header: CiHeader,
        /// `true` for Discovery Reply (sub-ID#2 0x71), `false` for the inquiry.
        is_reply: bool,
        /// Advertised identity and capabilities.
        data: DiscoveryData,
    },
    /// Invalidate MUID: tells a device its MUID collided and it must pick a new
    /// one (M2-101 §5.9). Carries the target MUID.
    InvalidateMuid {
        /// Addressing preamble.
        header: CiHeader,
        /// The MUID being invalidated: the colliding one, not the sender's.
        target: Muid,
    },
    /// NAK: a device rejects a CI message it cannot handle (M2-101 §5.11).
    Nak {
        /// Addressing preamble.
        header: CiHeader,
        /// Which message was rejected, and why.
        nak: Nak,
    },
}

impl CiMessage {
    /// The message's preamble.
    #[must_use]
    pub fn header(&self) -> &CiHeader {
        match self {
            CiMessage::Discovery { header, .. }
            | CiMessage::InvalidateMuid { header, .. }
            | CiMessage::Nak { header, .. } => header,
        }
    }

    /// The CI byte payload: the bytes carried *inside* a SysEx, starting at
    /// `0x7E`, without the `0xF0` and `0xF7` delimiters.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            CiMessage::Discovery {
                header,
                is_reply,
                data,
            } => {
                let sub_id2 = if *is_reply {
                    SUB_ID2_DISCOVERY_REPLY
                } else {
                    SUB_ID2_DISCOVERY
                };
                header.encode(sub_id2, &mut out);
                data.encode_body(*is_reply, &mut out);
            }
            CiMessage::InvalidateMuid { header, target } => {
                header.encode(SUB_ID2_INVALIDATE_MUID, &mut out);
                out.extend_from_slice(&target.to_bytes());
            }
            CiMessage::Nak { header, nak } => {
                header.encode(SUB_ID2_NAK, &mut out);
                nak.encode_body(&mut out);
            }
        }
        out
    }

    /// Decodes a CI byte payload (as [`encode`](Self::encode) produces, or a
    /// `tutti_ump::SysexAssembler` completed) into a typed message, or `None`
    /// if the bytes are not a CI message this codec models.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<CiMessage> {
        let (header, sub_id2, body_at) = CiHeader::decode(bytes)?;
        let body = &bytes[body_at..];
        match sub_id2 {
            SUB_ID2_DISCOVERY => Some(CiMessage::Discovery {
                header,
                is_reply: false,
                data: DiscoveryData::decode_body(false, body)?,
            }),
            SUB_ID2_DISCOVERY_REPLY => Some(CiMessage::Discovery {
                header,
                is_reply: true,
                data: DiscoveryData::decode_body(true, body)?,
            }),
            SUB_ID2_INVALIDATE_MUID => {
                let b: [u8; 4] = body.get(..4)?.try_into().ok()?;
                Some(CiMessage::InvalidateMuid {
                    header,
                    target: Muid::from_bytes(b)?,
                })
            }
            SUB_ID2_NAK => Some(CiMessage::Nak {
                header,
                nak: Nak::decode_body(body)?,
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests;
