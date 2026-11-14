//! The Property Exchange Capabilities exchange (M2-101 §8.5, §8.6) and the
//! sub-ID#2 messages Property Exchange travels in (M2-101 §8.1).
//!
//! The two devices report how many requests each takes at once and which
//! version of the Common Rules for Property Exchange it follows. The reply
//! to the Inquiry holds the same two fields as the Inquiry.

use std::fmt;

use tutti_ump::U7;

use super::chunk::PeChunk;
use super::flow::PeMessageKind;
use super::limits::NotInRange;

/// Sub-ID#2: Inquiry: Property Exchange Capabilities (M2-101 §8.5).
pub const SUB_ID2_PE_CAPABILITIES_INQUIRY: u8 = 0x30;
/// Sub-ID#2: Reply to Property Exchange Capabilities (M2-101 §8.6).
pub const SUB_ID2_PE_CAPABILITIES_REPLY: u8 = 0x31;
/// Sub-ID#2: Notify (M2-101 §8.13, Table 39): superseded by ACK and NAK, and
/// still read (M2-103 §12).
pub const SUB_ID2_PE_NOTIFY: u8 = 0x3F;

/// The Number of Simultaneous Property Exchange Requests Supported: 1 to 127.
///
/// The field is one 7-bit byte, so the count stops at 127 although there are 128
/// Request IDs (M2-101 Tables 30 and 32, M2-103 §5.3); a device that supports one
/// request, the default (§8.4), reports 1. A device uses no more requests than
/// the other reports (M2-103 §5.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "u8", into = "u8"))]
pub struct SimultaneousRequests(u8);

impl SimultaneousRequests {
    /// The fewest, 1.
    pub const MIN: u8 = 1;
    /// The most a 7-bit field reports, 127.
    pub const MAX: u8 = 127;
    /// One request at a time, the default of a device (M2-101 §8.4).
    pub const ONE: Self = Self(1);

    /// The count, or `None` for 0 or past 127.
    #[must_use]
    pub const fn new(raw: u8) -> Option<Self> {
        if raw >= Self::MIN && raw <= Self::MAX {
            Some(Self(raw))
        } else {
            None
        }
    }

    /// The count for a literal or a constant.
    ///
    /// # Panics
    ///
    /// For 0 or past 127; in a `const` that is a compile error.
    #[must_use]
    #[track_caller]
    pub const fn of(raw: u8) -> Self {
        match Self::new(raw) {
            Some(count) => count,
            None => panic!("a simultaneous request count is 1 to 127"),
        }
    }

    /// The count.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl TryFrom<u8> for SimultaneousRequests {
    type Error = NotInRange<u8>;

    fn try_from(raw: u8) -> Result<Self, NotInRange<u8>> {
        Self::new(raw).ok_or(NotInRange {
            what: "simultaneous request count",
            raw,
        })
    }
}

impl From<SimultaneousRequests> for u8 {
    fn from(count: SimultaneousRequests) -> u8 {
        count.0
    }
}

impl fmt::Display for SimultaneousRequests {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The version of the Common Rules for MIDI-CI Property Exchange a device
/// follows: the Property Exchange Major and Minor Version fields (M2-101 §8.5,
/// Table 31). Both are 7-bit bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PeVersion {
    /// The Major Version.
    pub major: U7,
    /// The Minor Version.
    pub minor: U7,
}

impl PeVersion {
    /// Common Rules versions 1.0 and 1.1, which Table 31 gives `0x00` and `0x00`.
    pub const COMMON_RULES_1_1: Self = Self {
        major: U7::of(0),
        minor: U7::of(0),
    };
}

/// The fields of a Property Exchange Capabilities message, an Inquiry or its
/// Reply (M2-101 Tables 30 and 32).
///
/// A device whose Message Format Version is 1 stops after the request count: the
/// version fields were added in Message Format Version 2, so `version` is
/// `None` for such a device, and a device writes the fields it has.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PeCapabilities {
    /// How many requests the sender takes at once.
    pub requests: SimultaneousRequests,
    /// The Property Exchange version the sender follows, absent from a Message
    /// Format Version 1 message.
    pub version: Option<PeVersion>,
}

impl PeCapabilities {
    /// A device that takes `requests` at once and follows Common Rules 1.1,
    /// in the form of Message Format Version 2.
    #[must_use]
    pub const fn new(requests: SimultaneousRequests) -> Self {
        Self {
            requests,
            version: Some(PeVersion::COMMON_RULES_1_1),
        }
    }

    fn encode_body(&self, out: &mut impl Extend<u8>) {
        out.extend([self.requests.get()]);
        if let Some(version) = self.version {
            out.extend([version.major.get(), version.minor.get()]);
        }
    }

    /// The fields of a body, or `None` for a count of 0, a byte with its top bit
    /// set, or one version byte without the other. Bytes after the version are
    /// the fields of a later Message Format Version and are ignored (M2-101 §5.3).
    fn decode_body(body: &[u8]) -> Option<Self> {
        let (requests, version) = match body {
            [requests] => (*requests, None),
            [requests, major, minor, ..] => (
                *requests,
                Some(PeVersion {
                    major: U7::new(*major)?,
                    minor: U7::new(*minor)?,
                }),
            ),
            _ => return None,
        };
        Some(Self {
            requests: SimultaneousRequests::new(requests)?,
            version,
        })
    }
}

/// A Property Exchange message of MIDI-CI: the Capabilities pair, one chunk
/// of an inquiry or a reply, or the superseded Notify (M2-101 §8.1, §8.13).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeCiMessage {
    /// Inquiry: Property Exchange Capabilities, `0x30`.
    CapabilitiesInquiry(PeCapabilities),
    /// Reply to Property Exchange Capabilities, `0x31`.
    CapabilitiesReply(PeCapabilities),
    /// One chunk of Get, Set or Subscription, or of its reply, `0x34` to `0x39`.
    Chunk {
        /// Which of the six messages.
        kind: PeMessageKind,
        /// The chunk.
        chunk: PeChunk,
    },
    /// Notify, `0x3F`: the status of a transaction, by Request ID, in the
    /// fields of a chunk (M2-101 Table 39). A device should not send one
    /// (§8.13); one received is read as M2-103 §12 says
    /// ([`PeInitiator`](crate::PeInitiator) does), with its status in the
    /// Header Data ([`PeChunk::notify`] builds one, and
    /// [`PeChunk::notified`] reads it).
    Notify(PeChunk),
}

impl PeCiMessage {
    /// The sub-ID#2.
    #[must_use]
    pub const fn sub_id2(&self) -> u8 {
        match self {
            Self::CapabilitiesInquiry(_) => SUB_ID2_PE_CAPABILITIES_INQUIRY,
            Self::CapabilitiesReply(_) => SUB_ID2_PE_CAPABILITIES_REPLY,
            Self::Chunk { kind, .. } => kind.sub_id2(),
            Self::Notify(_) => SUB_ID2_PE_NOTIFY,
        }
    }

    pub(crate) fn encode_body(&self, out: &mut impl Extend<u8>) {
        match self {
            Self::CapabilitiesInquiry(caps) | Self::CapabilitiesReply(caps) => {
                caps.encode_body(out);
            }
            Self::Chunk { chunk, .. } | Self::Notify(chunk) => out.extend(chunk.encode_fields()),
        }
    }

    /// The message of sub-ID#2 `sub_id2` (`0x30`, `0x31`, `0x34` to `0x39` or `0x3F`) with
    /// body `body`, or `None` when the body is malformed.
    pub(crate) fn decode(sub_id2: u8, body: &[u8]) -> Option<Self> {
        match sub_id2 {
            SUB_ID2_PE_CAPABILITIES_INQUIRY => {
                PeCapabilities::decode_body(body).map(Self::CapabilitiesInquiry)
            }
            SUB_ID2_PE_CAPABILITIES_REPLY => {
                PeCapabilities::decode_body(body).map(Self::CapabilitiesReply)
            }
            SUB_ID2_PE_NOTIFY => PeChunk::parse_fields(body).ok().map(Self::Notify),
            _ => Some(Self::Chunk {
                kind: PeMessageKind::from_sub_id2(sub_id2)?,
                chunk: PeChunk::parse_fields(body).ok()?,
            }),
        }
    }
}

#[cfg(test)]
mod tests;
