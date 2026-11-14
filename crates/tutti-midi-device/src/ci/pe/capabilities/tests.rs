//! The Capabilities messages and the Property Exchange messages in CI (M2-101
//! §8). Each test names the property of the spec it holds.

use super::*;
use crate::MaxSysexSize;
use crate::ci::pe::{
    ChunkNo, ChunkPosition, ChunkTotal, PeHeader, PeRequest, PeRequestId, PeStatus, ResourceName,
};
use crate::ci::testing::decoded;
use crate::ci::{CiHeader, CiMessage, CiVersion, Decoded, Muid, Unread, UnreadReason};

fn pe(header: CiHeader, message: PeCiMessage) -> CiMessage {
    CiMessage::PropertyExchange {
        header,
        to: Muid::of(2),
        message,
    }
}

// M2-101 Table 30: Device ID 7F, sub-ID#2 0x30, the two MUIDs, the count of
// simultaneous requests, and, from Message Format Version 2, the two version
// bytes; Table 31 gives Common Rules 1.1 the version 0x00 0x00.
#[test]
fn the_capabilities_inquiry_lays_out_table_30() {
    let message = pe(
        CiHeader::new(Muid::of(1)),
        PeCiMessage::CapabilitiesInquiry(PeCapabilities {
            requests: SimultaneousRequests::of(4),
            version: Some(PeVersion::COMMON_RULES_1_1),
        }),
    );
    assert_eq!(
        message.encode(),
        [
            0x7E, 0x7F, 0x0D, 0x30, 0x02, 1, 0, 0, 0, 2, 0, 0, 0, // preamble and MUIDs
            4, 0, 0, // count, major, minor
        ]
    );
    assert_eq!(decoded(&message.encode()), message);
}

// M2-101 Table 32: the reply has the same fields, under sub-ID#2 0x31.
#[test]
fn the_capabilities_reply_lays_out_table_32() {
    let message = pe(
        CiHeader::new(Muid::of(1)),
        PeCiMessage::CapabilitiesReply(PeCapabilities::new(SimultaneousRequests::of(127))),
    );
    let bytes = message.encode();
    assert_eq!(bytes[3], 0x31);
    assert_eq!(bytes[13..], [127, 0, 0]);
    assert_eq!(decoded(&bytes), message);
}

// M2-101 Tables 30 and 32: the version fields were added in Message Format
// Version 2, so a version 1 message ends after the count and its version is
// absent; §5.3 has a receiver read the fields of the version it received.
#[test]
fn a_version_1_capabilities_message_has_no_version() {
    let header = CiHeader {
        ci_version: CiVersion::of(1),
        ..CiHeader::new(Muid::of(1))
    };
    let message = pe(
        header,
        PeCiMessage::CapabilitiesReply(PeCapabilities {
            requests: SimultaneousRequests::of(2),
            version: None,
        }),
    );
    let bytes = message.encode();
    assert_eq!(bytes.len(), 14);
    assert_eq!(bytes[13], 2);
    assert_eq!(decoded(&bytes), message);
}

// §5.3: fields appended by a later version are ignored.
#[test]
fn fields_a_later_version_appends_are_ignored() {
    let mut bytes = pe(
        CiHeader::new(Muid::of(1)),
        PeCiMessage::CapabilitiesReply(PeCapabilities::new(SimultaneousRequests::of(2))),
    )
    .encode();
    bytes.extend([5, 6]);
    let CiMessage::PropertyExchange {
        message: PeCiMessage::CapabilitiesReply(caps),
        ..
    } = decoded(&bytes)
    else {
        panic!("not a reply");
    };
    assert_eq!(caps, PeCapabilities::new(SimultaneousRequests::of(2)));
}

// The count is 1 to 127 (a device supports at least the one request of §8.4),
// and the version is both bytes or neither.
#[test]
fn a_capabilities_body_of_no_requests_or_half_a_version_is_malformed() {
    let base = pe(
        CiHeader::new(Muid::of(1)),
        PeCiMessage::CapabilitiesInquiry(PeCapabilities::new(SimultaneousRequests::of(2))),
    )
    .encode();
    for body in [vec![], vec![0], vec![0, 0, 0], vec![2, 0], vec![2, 0x80, 0]] {
        let mut bytes = base[..13].to_vec();
        bytes.extend(body.clone());
        assert!(
            matches!(
                CiMessage::decode(&bytes),
                Decoded::Unread(Unread {
                    reason: UnreadReason::Malformed,
                    ..
                })
            ),
            "{body:?}"
        );
    }
}

// M2-101 Table 33: Request ID, Header Data length, Header Data, number of chunks,
// number of this chunk, Property Data length and Property Data, each count 14 bits.
#[test]
fn a_get_inquiry_lays_out_table_33() {
    let header = PeHeader::Request(PeRequest::new(ResourceName::new("State").unwrap()));
    let text = br#"{"resource":"State"}"#;
    let chunk = PeChunk::new(
        PeRequestId::of(9),
        ChunkTotal::Known(ChunkNo::FIRST),
        ChunkPosition::Number(ChunkNo::FIRST),
        header.encode(MaxSysexSize::of(512)).unwrap(),
        vec![],
    )
    .unwrap();
    let message = pe(
        CiHeader::new(Muid::of(1)),
        PeCiMessage::Chunk {
            kind: PeMessageKind::GET_INQUIRY,
            chunk,
        },
    );
    let mut expected = vec![
        0x7E,
        0x7F,
        0x0D,
        0x34,
        0x02,
        1,
        0,
        0,
        0,
        2,
        0,
        0,
        0, // preamble and MUIDs
        9, // Request ID
        u8::try_from(text.len()).unwrap(),
        0,
    ];
    expected.extend(text);
    expected.extend([1, 0, 1, 0, 0, 0]); // 1 of 1, data length 0
    assert_eq!(message.encode(), expected);
    assert_eq!(decoded(&expected), message);
}

// Each of the six chunked messages is the sub-ID#2 of its kind (§8.1).
#[test]
fn each_chunked_message_is_sent_under_its_sub_id2() {
    for sub_id2 in 0x34..=0x39u8 {
        let kind = PeMessageKind::from_sub_id2(sub_id2).unwrap();
        let chunk = PeChunk::new(
            PeRequestId::of(0),
            ChunkTotal::Known(ChunkNo::FIRST),
            ChunkPosition::Number(ChunkNo::FIRST),
            vec![],
            vec![],
        )
        .unwrap();
        let bytes = pe(
            CiHeader::new(Muid::of(1)),
            PeCiMessage::Chunk { kind, chunk },
        )
        .encode();
        assert_eq!(bytes[3], sub_id2);
    }
}

// A sub-ID#2 no message uses (0x3E) is not read; the PE messages go to one device
// (§8), so one sent to everyone is the wrong destination.
#[test]
fn an_unused_sub_id2_is_unsupported_and_a_broadcast_pe_message_is_misaddressed() {
    let mut bytes = pe(
        CiHeader::new(Muid::of(1)),
        PeCiMessage::CapabilitiesInquiry(PeCapabilities::new(SimultaneousRequests::ONE)),
    )
    .encode();
    bytes[3] = 0x3E;
    assert!(matches!(
        CiMessage::decode(&bytes),
        Decoded::Unread(Unread {
            reason: UnreadReason::UnsupportedMessage,
            ..
        })
    ));
    bytes[3] = 0x30;
    bytes[9..13].copy_from_slice(&[0x7F; 4]);
    assert!(matches!(
        CiMessage::decode(&bytes),
        Decoded::Unread(Unread {
            reason: UnreadReason::WrongDestination,
            ..
        })
    ));
}

#[cfg(feature = "serde")]
#[test]
fn the_request_count_is_a_range_checked_value() {
    use tutti_fixtures::{RangeContract, check_range_contract};
    check_range_contract(&RangeContract {
        new: SimultaneousRequests::new,
        get: SimultaneousRequests::get,
        of: Some(SimultaneousRequests::of),
        valid: &[1, 2, 127],
        invalid: &[0, 128, 255],
    });
}

// M2-101 Table 39: Notify has the fields of a chunk (Request ID, Header Data
// with its length, the two chunk counts, Property Data with its length) under
// sub-ID#2 0x3F, and goes to one device.
#[test]
fn notify_lays_out_table_39_and_goes_to_one_device() {
    let chunk = PeChunk::notify(PeRequestId::of(5), PeStatus::Terminate);
    let message = pe(
        CiHeader::new(Muid::of(1)),
        PeCiMessage::Notify(chunk.clone()),
    );
    let text = br#"{"status":144}"#;
    let mut expected = vec![
        0x7E,
        0x7F,
        0x0D,
        0x3F,
        0x02,
        1,
        0,
        0,
        0,
        2,
        0,
        0,
        0, // preamble and MUIDs
        5, // Request ID
        u8::try_from(text.len()).unwrap(),
        0,
    ];
    expected.extend(text);
    expected.extend([1, 0, 1, 0, 0, 0]); // 1 of 1, data length 0
    assert_eq!(message.encode(), expected);
    assert_eq!(decoded(&expected), message);
    assert_eq!(chunk.notified(), Some(PeStatus::Terminate));

    // Sent to everyone, it is misaddressed, not a Notify.
    let mut broadcast = expected;
    broadcast[9..13].copy_from_slice(&[0x7F; 4]);
    assert!(matches!(
        CiMessage::decode(&broadcast),
        Decoded::Unread(Unread {
            reason: UnreadReason::WrongDestination,
            ..
        })
    ));
}

// The three statuses a Notify carries (M2-103 §12) survive the round trip, and
// Header Data that is not a header with a status is not one.
#[test]
fn a_notify_status_is_read_back_from_its_header_data() {
    for status in [
        PeStatus::TimeoutWait,
        PeStatus::Terminate,
        PeStatus::TimedOut,
    ] {
        assert_eq!(
            PeChunk::notify(PeRequestId::of(0), status).notified(),
            Some(status)
        );
    }
    let junk = PeChunk::new(
        PeRequestId::of(0),
        ChunkTotal::Known(ChunkNo::FIRST),
        ChunkPosition::Number(ChunkNo::FIRST),
        br#"{"status":777}"#.to_vec(),
        vec![],
    )
    .unwrap();
    assert_eq!(junk.notified(), None);
}
