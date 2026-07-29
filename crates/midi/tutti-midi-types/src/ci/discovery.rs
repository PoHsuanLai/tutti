//! MIDI-CI Discovery, Invalidate MUID, and NAK — the mandatory core (M2-101 §5.2/§5.5/§5.6).
//!
//! Discovery is the opening handshake: an initiator broadcasts its identity and
//! the CI categories it supports, and each responder replies in kind. The reply
//! shares the Discovery *body* format, so [`DiscoveryData`] serves both.

use std::vec::Vec;

use bitflags::bitflags;

/// Sub-ID#2: Discovery message (M2-101 §5.2).
pub const SUB_ID2_DISCOVERY: u8 = 0x70;
/// Sub-ID#2: Discovery Reply message.
pub const SUB_ID2_DISCOVERY_REPLY: u8 = 0x71;
/// Sub-ID#2: Invalidate MUID message (M2-101 §5.5).
pub const SUB_ID2_INVALIDATE_MUID: u8 = 0x7E;
/// Sub-ID#2: NAK message (M2-101 §5.6).
pub const SUB_ID2_NAK: u8 = 0x7F;

bitflags! {
    /// The CI capability categories a device advertises in Discovery
    /// (M2-101 §5.2, the "CI Category Supported" bitmap).
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

/// The body of a Discovery / Discovery Reply message (M2-101 §5.2): the sending
/// device's SysEx identity, the CI categories it supports, and its receive
/// buffer size. Both directions share this shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiscoveryData {
    /// 3-byte SysEx device manufacturer id.
    pub manufacturer: [u8; 3],
    /// 2-byte device family (little-endian 7-bit pair on the wire).
    pub family: u16,
    /// 2-byte device family model number.
    pub family_model: u16,
    /// 4-byte software/firmware revision.
    pub software_revision: [u8; 4],
    /// CI categories this device supports.
    pub categories: CiCategories,
    /// Maximum SysEx message size the device can receive (28-bit).
    pub max_sysex_size: u32,
}

impl DiscoveryData {
    /// Body length in bytes: 3 mfr + 2 family + 2 model + 4 revision + 1
    /// categories + 4 max-size.
    pub const BODY_LEN: usize = 3 + 2 + 2 + 4 + 1 + 4;

    pub(super) fn encode_body(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.manufacturer);
        out.push((self.family & 0x7F) as u8);
        out.push(((self.family >> 7) & 0x7F) as u8);
        out.push((self.family_model & 0x7F) as u8);
        out.push(((self.family_model >> 7) & 0x7F) as u8);
        out.extend_from_slice(&self.software_revision);
        out.push(self.categories.bits());
        // 28-bit max size as four little-endian 7-bit bytes.
        out.push((self.max_sysex_size & 0x7F) as u8);
        out.push(((self.max_sysex_size >> 7) & 0x7F) as u8);
        out.push(((self.max_sysex_size >> 14) & 0x7F) as u8);
        out.push(((self.max_sysex_size >> 21) & 0x7F) as u8);
    }

    pub(super) fn decode_body(b: &[u8]) -> Option<DiscoveryData> {
        if b.len() < Self::BODY_LEN {
            return None;
        }
        Some(DiscoveryData {
            manufacturer: [b[0], b[1], b[2]],
            family: (b[3] as u16 & 0x7F) | ((b[4] as u16 & 0x7F) << 7),
            family_model: (b[5] as u16 & 0x7F) | ((b[6] as u16 & 0x7F) << 7),
            software_revision: [b[7], b[8], b[9], b[10]],
            categories: CiCategories::from_bits_truncate(b[11]),
            max_sysex_size: (b[12] as u32 & 0x7F)
                | ((b[13] as u32 & 0x7F) << 7)
                | ((b[14] as u32 & 0x7F) << 14)
                | ((b[15] as u32 & 0x7F) << 21),
        })
    }
}

/// The body of a NAK message (M2-101 §5.6). CI 1.2 carries a status code and a
/// short reason; older devices sent an empty NAK, so the fields default to zero
/// when absent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Nak {
    /// The sub-ID#2 of the message being NAK'd (0 if unknown).
    pub nak_sub_id2: u8,
    /// Status code for *why* (M2-101 status list; 0 = generic).
    pub status_code: u8,
    /// Status data (secondary detail; 0 when unused).
    pub status_data: u8,
}

impl Nak {
    /// Generic failure, no further detail (M2-101 Table 16).
    pub const STATUS_NAK: u8 = 0x00;
    /// "MIDI-CI message not supported" — we don't implement this message at all.
    pub const STATUS_MESSAGE_NOT_SUPPORTED: u8 = 0x01;
    /// "MIDI-CI version not supported".
    pub const STATUS_VERSION_NOT_SUPPORTED: u8 = 0x02;
    /// "Channel/Group/Function Block Not in use".
    pub const STATUS_NOT_IN_USE: u8 = 0x03;
    /// "Profile not supported on the requested Channel, Group, or Function Block".
    pub const STATUS_PROFILE_NOT_SUPPORTED: u8 = 0x04;
    /// "Error occurred, please retry" — unlike the 0x00-0x1F codes, this one
    /// tells the initiator a retry is worthwhile.
    pub const STATUS_RETRY: u8 = 0x40;
    /// "Message was malformed".
    pub const STATUS_MALFORMED: u8 = 0x41;
    /// "Timeout has occurred".
    pub const STATUS_TIMEOUT: u8 = 0x42;

    /// A NAK for `nak_sub_id2` with a specific Table 16 status code.
    ///
    /// Prefer a precise code over [`Self::STATUS_NAK`]: Table 16 splits
    /// "Do Not Retry" (0x00-0x1F) from "Retry is recommended" (0x40-0x5F), and a
    /// generic 0x00 tells the peer only that something went wrong.
    pub fn new(nak_sub_id2: u8, status_code: u8) -> Self {
        Self {
            nak_sub_id2,
            status_code,
            status_data: 0,
        }
    }

    pub(super) fn encode_body(&self, out: &mut Vec<u8>) {
        out.push(self.nak_sub_id2);
        out.push(self.status_code);
        out.push(self.status_data);
    }

    pub(super) fn decode_body(b: &[u8]) -> Option<Nak> {
        // Tolerate an empty (pre-1.2) NAK.
        Some(Nak {
            nak_sub_id2: b.first().copied().unwrap_or(0),
            status_code: b.get(1).copied().unwrap_or(0),
            status_data: b.get(2).copied().unwrap_or(0),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;

    fn header() -> CiHeader {
        CiHeader {
            device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
            ci_version: CI_VERSION,
            source: Muid(0x0123_4567),
            destination: Muid::BROADCAST,
        }
    }

    fn sample_discovery() -> DiscoveryData {
        DiscoveryData {
            manufacturer: [0x00, 0x21, 0x09],
            family: 0x1234,
            family_model: 0x0055,
            software_revision: [1, 2, 3, 4],
            categories: CiCategories::PROFILE_CONFIGURATION | CiCategories::PROPERTY_EXCHANGE,
            max_sysex_size: 512,
        }
    }

    #[test]
    fn discovery_round_trips_over_sysex7() {
        let msg = CiMessage::Discovery {
            header: header(),
            is_reply: false,
            data: sample_discovery(),
        };
        let mut events = Vec::new();
        ci_to_sysex7(0, &msg, &mut events);
        let back = sysex7_to_ci(&events).expect("reassembles");
        assert_eq!(back, msg);
    }

    #[test]
    fn discovery_reply_keeps_is_reply_flag() {
        let msg = CiMessage::Discovery {
            header: header(),
            is_reply: true,
            data: sample_discovery(),
        };
        let bytes = msg.encode();
        let back = CiMessage::decode(&bytes).expect("decodes");
        match back {
            CiMessage::Discovery { is_reply, data, .. } => {
                assert!(is_reply);
                assert_eq!(data, sample_discovery());
            }
            other => panic!("expected Discovery reply, got {other:?}"),
        }
    }

    #[test]
    fn nak_round_trips() {
        let msg = CiMessage::Nak {
            header: header(),
            nak: Nak {
                nak_sub_id2: SUB_ID2_DISCOVERY,
                status_code: 0x02,
                status_data: 0x00,
            },
        };
        let back = CiMessage::decode(&msg.encode()).expect("decodes");
        assert_eq!(back, msg);
    }

    #[test]
    fn categories_survive_round_trip() {
        let d = sample_discovery();
        let mut body = Vec::new();
        d.encode_body(&mut body);
        let back = DiscoveryData::decode_body(&body).expect("decodes");
        assert!(back
            .categories
            .contains(CiCategories::PROFILE_CONFIGURATION));
        assert!(back.categories.contains(CiCategories::PROPERTY_EXCHANGE));
        assert!(!back.categories.contains(CiCategories::PROCESS_INQUIRY));
    }
}
