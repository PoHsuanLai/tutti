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

/// The "no Function Block" value for [`DiscoveryData::function_block`]
/// (M2-101 §5.6.2: "If the MIDI-CI Device is not associated with a Function
/// Block (for example, is connected by a MIDI 1.0 byte stream), then set the
/// value to 0x7F").
pub const NO_FUNCTION_BLOCK: u8 = 0x7F;

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
/// shapes; see [`encode_body`](Self::encode_body).
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
    /// **Output Path ID** (Message Version 2). On a Discovery this is the
    /// initiator's own path id — §5.5.4: "Initiators that have multiple MIDI Out
    /// connections should use a unique Output Path ID for each connection." On a
    /// reply it echoes the id from the Discovery that prompted it, which §5.6.1
    /// makes mandatory: "The Reply to Discovery shall return the same Output
    /// Path ID provided in the originating Discovery Message."
    pub output_path_id: u8,
    /// **Function Block** this responder represents (Message Version 2, *reply
    /// only*), or [`NO_FUNCTION_BLOCK`] if it represents none.
    ///
    /// §5.6.2: "The Reply to Discovery shall declare the number of the Function
    /// Block that this Responder represents. This allows an Initiator to tie a
    /// Function Block to a MIDI-CI Device." Ignored when encoding a Discovery,
    /// which has no such field.
    pub function_block: u8,
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
    /// id 0 and [`NO_FUNCTION_BLOCK`]. Use the field setters for a device that
    /// has several MIDI Out connections or sits behind a Function Block.
    pub fn new(
        manufacturer: [u8; 3],
        family: u16,
        family_model: u16,
        software_revision: [u8; 4],
        categories: CiCategories,
        max_sysex_size: u32,
    ) -> Self {
        Self {
            manufacturer,
            family,
            family_model,
            software_revision,
            categories,
            max_sysex_size,
            output_path_id: 0,
            function_block: NO_FUNCTION_BLOCK,
        }
    }

    /// Encode the body. `is_reply` selects Table 8's trailer (path instance id +
    /// function block) over Table 6's (path id alone) — the one place the two
    /// directions differ on the wire.
    pub(super) fn encode_body(&self, is_reply: bool, out: &mut Vec<u8>) {
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
        out.push(self.output_path_id & 0x7F);
        if is_reply {
            out.push(self.function_block & 0x7F);
        }
    }

    /// Decode a Discovery or Reply body.
    ///
    /// The Version-2 trailer is read when present and defaulted when absent, so
    /// a Message Format Version 1 peer still decodes: §5.4 requires a receiver
    /// to "process the fields, values, and bits defined in the received version"
    /// when it is lower than its own. A missing Function Block reads as
    /// [`NO_FUNCTION_BLOCK`] rather than block 0, which would be a lie.
    pub(super) fn decode_body(is_reply: bool, b: &[u8]) -> Option<DiscoveryData> {
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
            output_path_id: b.get(Self::BODY_LEN).copied().unwrap_or(0),
            function_block: if is_reply {
                b.get(Self::BODY_LEN + 1)
                    .copied()
                    .unwrap_or(NO_FUNCTION_BLOCK)
            } else {
                NO_FUNCTION_BLOCK
            },
        })
    }
}

/// The body of a NAK message (M2-101 Table 15).
///
/// Everything below `nak_sub_id2` was added in Message Format Version 2. Older
/// devices sent an empty NAK, so decoding defaults each absent field.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Nak {
    /// "Original Transaction Sub-ID#2 Classification" — the sub-ID#2 of the
    /// message being NAK'd (0 if unknown). §5.11.1: it "is used, along with
    /// other fields, to link the NAK message with the original Transaction".
    pub nak_sub_id2: u8,
    /// Status code for *why* (Table 16; 0 = generic).
    pub status_code: u8,
    /// Status data — the secondary detail some Table 16 codes require, 0 when
    /// "no additional information is needed".
    pub status_data: u8,
    /// "NAK details for each Sub ID Classification": 5 bytes whose meaning
    /// depends on `nak_sub_id2`. Zeroed when a transaction has no detail to add.
    pub details: [u8; 5],
    /// Human-readable reason, sent as a length-prefixed UTF-8 run ("2 bytes
    /// Message Length (ml) (LSB first)" then "ml bytes Message Text"). Empty for
    /// no text.
    pub message: String,
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
            details: [0; 5],
            message: String::new(),
        }
    }

    /// Attach a human-readable reason (Table 15's Message Text).
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = message.into();
        self
    }

    /// Attach the 5-byte per-classification detail block.
    pub fn with_details(mut self, details: [u8; 5]) -> Self {
        self.details = details;
        self
    }

    pub(super) fn encode_body(&self, out: &mut Vec<u8>) {
        out.push(self.nak_sub_id2);
        out.push(self.status_code);
        out.push(self.status_data);
        out.extend_from_slice(&self.details);
        // Message Text is 7-bit SysEx data, so only ASCII survives; anything
        // else is dropped rather than truncated mid-character.
        let text: Vec<u8> = self
            .message
            .bytes()
            .filter(|b| b.is_ascii() && *b < 0x80)
            .collect();
        let len = text.len().min(0x3FFF) as u16;
        out.push((len & 0x7F) as u8);
        out.push(((len >> 7) & 0x7F) as u8);
        out.extend_from_slice(&text[..len as usize]);
    }

    pub(super) fn decode_body(b: &[u8]) -> Option<Nak> {
        // Every field past `nak_sub_id2` is Version-2, and a pre-1.2 peer sends
        // an empty NAK — so each is defaulted rather than required.
        let mut details = [0u8; 5];
        for (i, slot) in details.iter_mut().enumerate() {
            *slot = b.get(3 + i).copied().unwrap_or(0);
        }
        let len = match (b.get(8), b.get(9)) {
            (Some(lo), Some(hi)) => ((*lo as usize) & 0x7F) | (((*hi as usize) & 0x7F) << 7),
            _ => 0,
        };
        let text = b.get(10..10 + len).unwrap_or(&[]);
        Some(Nak {
            nak_sub_id2: b.first().copied().unwrap_or(0),
            status_code: b.get(1).copied().unwrap_or(0),
            status_data: b.get(2).copied().unwrap_or(0),
            details,
            message: String::from_utf8_lossy(text).into_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;
    use tutti_types::MidiGroup;

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
            output_path_id: 0x05,
            function_block: 0x03,
        }
    }

    #[test]
    fn discovery_round_trips_over_sysex7() {
        // A Discovery carries no Function Block field (Table 6), so it decodes
        // as NO_FUNCTION_BLOCK regardless of what the struct held.
        let msg = CiMessage::Discovery {
            header: header(),
            is_reply: false,
            data: DiscoveryData {
                function_block: NO_FUNCTION_BLOCK,
                ..sample_discovery()
            },
        };
        let mut events = Vec::new();
        ci_to_sysex7(MidiGroup::FIRST, &msg, &mut events);
        let back = sysex7_to_ci(&events).expect("reassembles");
        assert_eq!(back, msg);
    }

    #[test]
    fn discovery_and_reply_have_the_version_2_trailers() {
        // Table 6 appends 1 byte Output Path ID; Table 8 appends Output Path
        // Instance ID *and* Function Block. The two directions differ by exactly
        // one byte, which is why the struct alone can't decide the encoding.
        let d = sample_discovery();

        let mut discovery = Vec::new();
        d.encode_body(false, &mut discovery);
        assert_eq!(discovery.len(), DiscoveryData::DISCOVERY_LEN);
        assert_eq!(
            discovery[DiscoveryData::BODY_LEN],
            0x05,
            "Output Path ID is the last Discovery byte"
        );

        let mut reply = Vec::new();
        d.encode_body(true, &mut reply);
        assert_eq!(reply.len(), DiscoveryData::REPLY_LEN);
        assert_eq!(reply[DiscoveryData::BODY_LEN], 0x05, "path id echoed");
        assert_eq!(
            reply[DiscoveryData::BODY_LEN + 1],
            0x03,
            "Function Block follows it"
        );

        // Both directions decode back to what they encoded.
        assert_eq!(
            DiscoveryData::decode_body(true, &reply).expect("reply decodes"),
            d
        );
        assert_eq!(
            DiscoveryData::decode_body(false, &discovery)
                .expect("discovery decodes")
                .output_path_id,
            0x05
        );
    }

    #[test]
    fn a_version_1_body_still_decodes() {
        // §5.4: a receiver must "process the fields, values, and bits defined in
        // the received version" when it is lower than its own. A v1 peer sends
        // the body with no Version-2 trailer at all.
        let d = sample_discovery();
        let mut body = Vec::new();
        d.encode_body(true, &mut body);
        body.truncate(DiscoveryData::BODY_LEN); // strip the v2 trailer

        let back = DiscoveryData::decode_body(true, &body).expect("v1 body decodes");
        assert_eq!(back.manufacturer, d.manufacturer);
        assert_eq!(back.max_sysex_size, d.max_sysex_size);
        assert_eq!(back.output_path_id, 0, "absent path id defaults to 0");
        assert_eq!(
            back.function_block, NO_FUNCTION_BLOCK,
            "an absent Function Block must not read as block 0"
        );
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
            nak: Nak::new(SUB_ID2_DISCOVERY, Nak::STATUS_VERSION_NOT_SUPPORTED),
        };
        let back = CiMessage::decode(&msg.encode()).expect("decodes");
        assert_eq!(back, msg);
    }

    #[test]
    fn nak_carries_its_version_2_details_and_text() {
        // Table 15's Version-2 trailer: 5 detail bytes, a 2-byte LSB-first
        // length, then that many text bytes. All of it was previously dropped.
        let nak = Nak::new(SUB_ID2_DISCOVERY, Nak::STATUS_MALFORMED)
            .with_details([1, 2, 3, 4, 5])
            .with_message("body one byte short");
        let msg = CiMessage::Nak {
            header: header(),
            nak: nak.clone(),
        };

        let back = CiMessage::decode(&msg.encode()).expect("decodes");
        match back {
            CiMessage::Nak { nak: got, .. } => {
                assert_eq!(got.status_code, Nak::STATUS_MALFORMED);
                assert_eq!(got.details, [1, 2, 3, 4, 5]);
                assert_eq!(got.message, "body one byte short");
            }
            other => panic!("expected NAK, got {other:?}"),
        }

        // The length prefix is two 7-bit LSB-first bytes, so text longer than
        // 127 chars survives — the case a single length byte would truncate.
        let long = "e".repeat(300);
        let msg = CiMessage::Nak {
            header: header(),
            nak: Nak::new(0x34, Nak::STATUS_RETRY).with_message(&long),
        };
        match CiMessage::decode(&msg.encode()).expect("decodes") {
            CiMessage::Nak { nak, .. } => assert_eq!(nak.message, long),
            other => panic!("expected NAK, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_pre_1_2_nak_still_decodes() {
        // Older devices sent a NAK with no body at all; every Version-2 field
        // defaults rather than the message being rejected.
        let nak = Nak::decode_body(&[]).expect("empty NAK decodes");
        assert_eq!(nak.nak_sub_id2, 0);
        assert_eq!(nak.status_code, Nak::STATUS_NAK);
        assert_eq!(nak.details, [0; 5]);
        assert!(nak.message.is_empty());

        // …and a v1-length NAK (the three original bytes) too.
        let nak = Nak::decode_body(&[SUB_ID2_DISCOVERY, 0x04, 0x00]).expect("v1 NAK decodes");
        assert_eq!(nak.status_code, Nak::STATUS_PROFILE_NOT_SUPPORTED);
        assert!(nak.message.is_empty());
    }

    #[test]
    fn categories_survive_round_trip() {
        let d = sample_discovery();
        let mut body = Vec::new();
        d.encode_body(true, &mut body);
        let back = DiscoveryData::decode_body(true, &body).expect("decodes");
        assert!(back
            .categories
            .contains(CiCategories::PROFILE_CONFIGURATION));
        assert!(back.categories.contains(CiCategories::PROPERTY_EXCHANGE));
        assert!(!back.categories.contains(CiCategories::PROCESS_INQUIRY));
    }
}
