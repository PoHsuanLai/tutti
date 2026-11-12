//! MIDI-CI Discovery and its reply: the body, the sub-IDs of every message the
//! codec models, and the two range-checked fields (M2-101 §5.5 and §5.6).
//!
//! Discovery is the opening handshake: an initiator broadcasts its identity and
//! the CI categories it supports, and each responder replies in kind. The reply
//! shares the Discovery *body* format, so [`DiscoveryData`] serves both.

use std::fmt;

use bitflags::bitflags;
use tutti_ump::{DeviceIdentity, FunctionBlockId, U7};

/// Sub-ID#2: Discovery message (M2-101 §5.5).
pub const SUB_ID2_DISCOVERY: u8 = 0x70;
/// Sub-ID#2: Discovery Reply message.
pub const SUB_ID2_DISCOVERY_REPLY: u8 = 0x71;
/// Sub-ID#2: Invalidate MUID message (M2-101 §5.9).
pub const SUB_ID2_INVALIDATE_MUID: u8 = 0x7E;
/// Sub-ID#2: ACK message (M2-101 §5.10).
pub const SUB_ID2_ACK: u8 = 0x7D;
/// Sub-ID#2: NAK message (M2-101 §5.11).
pub const SUB_ID2_NAK: u8 = 0x7F;

bitflags! {
    /// The CI capability categories a device advertises in Discovery
    /// (M2-101 §5.5.2, the "CI Category Supported" bitmap).
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct CiCategories: u8 {
        /// Supports Protocol Negotiation (deprecated in CI 1.2, kept for the bit).
        const PROTOCOL_NEGOTIATION = 1 << 1;
        /// Supports Profile Configuration.
        const PROFILE_CONFIGURATION = 1 << 2;
        /// Supports Property Exchange.
        const PROPERTY_EXCHANGE = 1 << 3;
        /// Supports Process Inquiry.
        const PROCESS_INQUIRY = 1 << 4;
    }
}

/// The wire value of "no Function Block", `0x7F` (M2-101 §5.6.2: "If the
/// MIDI-CI Device is not associated with a Function Block (for example, is
/// connected by a MIDI 1.0 byte stream), then set the value to 0x7F (no
/// Function Block)"). [`DiscoveryData::function_block`] is `None` for it.
const NO_FUNCTION_BLOCK_WIRE: u8 = 0x7F;

/// The Receivable Maximum SysEx Message Size (M2-101 §5.5.3): 28 bits, and at
/// least 128.
///
/// "All MIDI-CI Devices shall support System Exclusive message lengths of at
/// least 128 bytes. The allowed values in the Receivable Maximum SysEx Message
/// Size are 128 or greater." A device that can initiate Profile Configuration
/// or Property Exchange "shall support message lengths of at least 512 bytes":
/// whether a device is one is the caller's to say, so that floor is
/// [`INITIATOR_MIN`](Self::INITIATOR_MIN) and not checked here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "u32", into = "u32"))]
pub struct MaxSysexSize(u32);

impl MaxSysexSize {
    /// The smallest size, 128 bytes (§5.5.3).
    pub const MIN: u32 = 128;
    /// The smallest size of a device that initiates Profile Configuration or
    /// Property Exchange transactions, 512 bytes (§5.5.3).
    pub const INITIATOR_MIN: u32 = 512;
    /// The largest, 28 bits: four 7-bit bytes.
    pub const MAX: u32 = 0x0FFF_FFFF;

    /// The size, or `None` below 128 bytes or past 28 bits.
    #[must_use]
    pub const fn new(raw: u32) -> Option<Self> {
        if raw >= Self::MIN && raw <= Self::MAX {
            Some(Self(raw))
        } else {
            None
        }
    }

    /// The size for a literal or a constant.
    ///
    /// # Panics
    ///
    /// Below 128 or past 28 bits; in a `const` that is a compile error.
    #[must_use]
    #[track_caller]
    pub const fn of(raw: u32) -> Self {
        match Self::new(raw) {
            Some(size) => size,
            None => panic!("a receivable maximum SysEx size is 128 to 2^28 - 1"),
        }
    }

    /// The size in bytes.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// A raw value that is not a Receivable Maximum SysEx Message Size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotASysexSize(pub u32);

impl fmt::Display for NotASysexSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} is not a receivable SysEx size (128 to 2^28 - 1)",
            self.0
        )
    }
}

impl std::error::Error for NotASysexSize {}

impl TryFrom<u32> for MaxSysexSize {
    type Error = NotASysexSize;

    fn try_from(raw: u32) -> Result<Self, NotASysexSize> {
        Self::new(raw).ok_or(NotASysexSize(raw))
    }
}

impl From<MaxSysexSize> for u32 {
    fn from(size: MaxSysexSize) -> u32 {
        size.0
    }
}

/// The body of a Discovery / Discovery Reply message (M2-101 Tables 6 and 8):
/// the sending device's SysEx identity, the CI categories it supports, its
/// receive buffer size, and the Message-Version-2 path fields.
///
/// The two directions share every field up to `max_sysex_size` and then diverge,
/// which is why `function_block` is meaningful only on a reply:
///
/// - **Discovery** (Table 6) appends `1 byte Initiator's Output Path ID`.
/// - **Reply to Discovery** (Table 8) appends `1 byte Initiator's Output Path
///   Instance ID (from the Discovery message received)` **and** `1 byte Function
///   Block`.
///
/// Encoding therefore takes the direction rather than the struct carrying two
/// shapes; see `encode_body`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiscoveryData {
    /// The device's SysEx identity: the 3-byte manufacturer id, the family and
    /// model (each a little-endian pair of 7-bit bytes) and the 4-byte software
    /// revision, the same ones a Device Inquiry reply carries (M2-101 §5.5.1).
    pub identity: DeviceIdentity,
    /// CI categories this device supports.
    pub categories: CiCategories,
    /// Maximum SysEx message size the device can receive (§5.5.3).
    pub max_sysex_size: MaxSysexSize,
    /// **Output Path ID** (Message Version 2). On a Discovery this is the
    /// initiator's own path id — §5.5.4: "Initiators that have multiple MIDI Out
    /// connections should use a unique Output Path ID for each connection." On a
    /// reply it echoes the id from the Discovery that prompted it, which §5.6.1
    /// makes mandatory: "The Reply to Discovery shall return the same Output
    /// Path ID provided in the originating Discovery Message."
    pub output_path_id: U7,
    /// **Function Block** this responder represents (Message Version 2, *reply
    /// only*), or `None` (wire `0x7F`) if it represents none. A number the
    /// spec does not give a Function Block (32 to 126) is malformed.
    ///
    /// §5.6.2: "The Reply to Discovery shall declare the number of the Function
    /// Block that this Responder represents. This allows an Initiator to tie a
    /// Function Block to a MIDI-CI Device." Ignored when encoding a Discovery,
    /// which has no such field.
    pub function_block: Option<FunctionBlockId>,
}

impl DiscoveryData {
    /// Length of the fields common to both directions: 3 mfr + 2 family +
    /// 2 model + 4 revision + 1 categories + 4 max-size.
    pub const BODY_LEN: usize = 3 + 2 + 2 + 4 + 1 + 4;

    /// Byte length of a Discovery body (Table 6): the common fields plus the
    /// Output Path ID.
    pub const DISCOVERY_LEN: usize = Self::BODY_LEN + 1;

    /// Byte length of a Reply to Discovery body (Table 8): the common fields
    /// plus Output Path Instance ID and Function Block.
    pub const REPLY_LEN: usize = Self::BODY_LEN + 2;

    /// A body with the Message-Version-2 fields at their neutral values: path
    /// id 0 and no Function Block. Use the field setters for a device that
    /// has several MIDI Out connections or sits behind a Function Block.
    #[must_use]
    pub fn new(
        identity: DeviceIdentity,
        categories: CiCategories,
        max_sysex_size: MaxSysexSize,
    ) -> Self {
        Self {
            identity,
            categories,
            max_sysex_size,
            output_path_id: U7::default(),
            function_block: None,
        }
    }

    /// Encode the body. `is_reply` selects Table 8's trailer (path instance id +
    /// function block) over Table 6's (path id alone) — the one place the two
    /// directions differ on the wire.
    pub(crate) fn encode_body(&self, is_reply: bool, out: &mut impl Extend<u8>) {
        let id = &self.identity;
        out.extend(
            id.manufacturer
                .iter()
                .chain(&id.family)
                .chain(&id.model)
                .chain(&id.revision)
                .map(|b| b.get()),
        );
        out.extend(self.categories.bits().to_le_bytes());
        // 28-bit max size as four little-endian 7-bit bytes.
        let size = self.max_sysex_size.get();
        out.extend([0, 7, 14, 21].map(|shift| ((size >> shift) & 0x7F) as u8));
        out.extend([self.output_path_id.get()]);
        if is_reply {
            out.extend([self
                .function_block
                .map_or(NO_FUNCTION_BLOCK_WIRE, FunctionBlockId::get)]);
        }
    }

    /// Decode a Discovery or Reply body, or `None` when it is malformed: shorter
    /// than the common fields, a byte with its top bit set, a Receivable
    /// Maximum SysEx Message Size under 128 (§5.5.3), or a Function Block that
    /// is neither 0 to 31 nor `0x7F` (§5.6.2).
    ///
    /// The Version-2 trailer is read when present and defaulted when absent, so
    /// a Message Format Version 1 peer still decodes: §5.3 requires a receiver
    /// to "process the fields, values, and bits defined in the received version"
    /// when it is lower than its own. A missing Function Block reads as `None`
    /// rather than block 0, which would be a lie.
    pub(crate) fn decode_body(is_reply: bool, b: &[u8]) -> Option<DiscoveryData> {
        if b.len() < Self::BODY_LEN || b[..Self::BODY_LEN].iter().any(|&v| v & 0x80 != 0) {
            return None;
        }
        let max_sysex_size = MaxSysexSize::new(
            u32::from(b[12])
                | (u32::from(b[13]) << 7)
                | (u32::from(b[14]) << 14)
                | (u32::from(b[15]) << 21),
        )?;
        let output_path_id = match b.get(Self::BODY_LEN) {
            Some(&id) => U7::new(id)?,
            None => U7::default(),
        };
        let function_block = match (is_reply, b.get(Self::BODY_LEN + 1)) {
            (true, Some(&NO_FUNCTION_BLOCK_WIRE)) | (false, _) | (true, None) => None,
            (true, Some(&n)) => Some(FunctionBlockId::new(n)?),
        };
        Some(DiscoveryData {
            // Every byte of the common fields was checked 7-bit above.
            identity: DeviceIdentity {
                manufacturer: [b[0], b[1], b[2]].map(U7::masked),
                family: [b[3], b[4]].map(U7::masked),
                model: [b[5], b[6]].map(U7::masked),
                revision: [b[7], b[8], b[9], b[10]].map(U7::masked),
            },
            categories: CiCategories::from_bits_truncate(b[11]),
            max_sysex_size,
            output_path_id,
            function_block,
        })
    }
}

#[cfg(test)]
mod tests;
