use super::super::testing::{DST, decoded, header};
use super::super::*;
use super::*;
use tutti_ump::ManufacturerId;

fn is_malformed(body: &[u8], is_reply: bool) -> bool {
    DiscoveryData::decode_body(is_reply, body).is_none()
}

fn sample_discovery() -> DiscoveryData {
    DiscoveryData {
        identity: DeviceIdentity {
            manufacturer: ManufacturerId::of(0xA109),
            family: [0x34, 0x24].map(U7::of),
            model: [0x55, 0x00].map(U7::of),
            revision: [1, 2, 3, 4].map(U7::of),
        },
        categories: CiCategories::PROFILE_CONFIGURATION | CiCategories::PROPERTY_EXCHANGE,
        max_sysex_size: MaxSysexSize::of(512),
        output_path_id: Some(U7::of(0x05)),
        function_block: FunctionBlockId::new(3),
    }
}

#[test]
fn discovery_round_trips() {
    // A Discovery carries no Function Block field (Table 6), so it decodes
    // as no Function Block regardless of what the struct held.
    let msg = CiMessage::Discovery {
        header: header(),
        data: DiscoveryData {
            function_block: None,
            ..sample_discovery()
        },
    };
    let bytes = msg.encode();
    assert_eq!(bytes[3], SUB_ID2_DISCOVERY);
    assert_eq!(bytes.len(), 13 + 17);
    assert_eq!(decoded(&bytes), msg);
}

#[test]
fn discovery_and_reply_have_the_version_2_trailers() {
    // Table 6 appends 1 byte Output Path ID; Table 8 appends Output Path
    // Instance ID *and* Function Block. The two directions differ by exactly
    // one byte, which is why the struct alone can't decide the encoding.
    let d = sample_discovery();

    let mut discovery = Vec::new();
    d.encode_body(false, &mut discovery);
    assert_eq!(discovery.len(), 17);
    assert_eq!(
        discovery[16], 0x05,
        "Output Path ID is the last Discovery byte"
    );

    let mut reply = Vec::new();
    d.encode_body(true, &mut reply);
    assert_eq!(reply.len(), 18);
    assert_eq!(reply[16], 0x05, "path id echoed");
    assert_eq!(reply[16 + 1], 0x03, "Function Block follows it");

    // Both directions decode back to what they encoded.
    assert_eq!(
        DiscoveryData::decode_body(true, &reply).expect("reply decodes"),
        d
    );
    assert_eq!(
        DiscoveryData::decode_body(false, &discovery)
            .expect("discovery decodes")
            .output_path_id,
        Some(U7::of(0x05))
    );
}

// M2-101 Tables 6 and 8 mark the Output Path ID and the Function Block as added
// in Message Format Version 2, and §5.3, §5.4 have a receiver read a lower
// version with the fields of that version: a 1.1 body ends after the Receivable
// Maximum SysEx Message Size (16 bytes), and the fields it lacks are absent, not
// path 0 and not block 0.
#[test]
fn a_version_1_body_has_no_output_path_and_no_function_block() {
    let d = sample_discovery();
    for is_reply in [false, true] {
        let mut body = Vec::new();
        d.encode_body(is_reply, &mut body);
        body.truncate(16);
        let back = DiscoveryData::decode_body(is_reply, &body).expect("v1 body decodes");
        assert_eq!(back.identity, d.identity);
        assert_eq!(back.max_sysex_size, d.max_sysex_size);
        assert_eq!(back.output_path_id, None, "reply {is_reply}");
        assert_eq!(back.function_block, None, "reply {is_reply}");
    }
}

// A body written with no Output Path ID is the Version 1 body: 16 bytes, with no
// Function Block after it either.
#[test]
fn a_body_with_no_output_path_encodes_as_version_1() {
    let d = DiscoveryData {
        output_path_id: None,
        ..sample_discovery()
    };
    for is_reply in [false, true] {
        let mut body = Vec::new();
        d.encode_body(is_reply, &mut body);
        assert_eq!(body.len(), 16, "reply {is_reply}");
    }
}

// The Reply of a device that sends Message Format Version 1 is a whole message
// that decodes, and the path 0 the Version 2 field names is not what it holds.
#[test]
fn a_version_1_discovery_reply_message_is_read() {
    let d = sample_discovery();
    let mut bytes = CiMessage::DiscoveryReply {
        header: CiHeader {
            ci_version: CiVersion::of(0x01),
            ..header()
        },
        to: Muid::of(DST),
        data: d,
    }
    .encode();
    bytes.truncate(13 + 16);
    match decoded(&bytes) {
        CiMessage::DiscoveryReply { header, to, data } => {
            assert_eq!(header.ci_version, CiVersion::of(0x01));
            assert_eq!(to, Muid::of(DST));
            assert_eq!(data.output_path_id, None);
            assert_eq!(data.function_block, None);
            assert_eq!(data.categories, d.categories);
        }
        other => panic!("expected a Discovery Reply, got {other:?}"),
    }
}

// A body shorter than Version 1's has no Receivable Maximum SysEx Message Size
// or categories, which no version omits: it is malformed.
#[test]
fn a_reply_shorter_than_version_1_is_malformed() {
    let mut body = Vec::new();
    sample_discovery().encode_body(true, &mut body);
    for len in [0, 11, 12, 15] {
        assert!(is_malformed(&body[..len], true), "{len} bytes");
        assert!(is_malformed(&body[..len], false), "{len} bytes");
    }
    assert!(!is_malformed(&body[..16], true));
    let mut bytes = CiMessage::DiscoveryReply {
        header: header(),
        to: Muid::of(DST),
        data: sample_discovery(),
    }
    .encode();
    bytes.truncate(13 + 15);
    assert!(matches!(
        CiMessage::decode(&bytes),
        Decoded::Unread(Unread {
            reason: UnreadReason::Malformed,
            ..
        })
    ));
}

#[test]
fn discovery_reply_keeps_is_reply_flag() {
    let msg = CiMessage::DiscoveryReply {
        header: header(),
        to: Muid::of(DST),
        data: sample_discovery(),
    };
    let bytes = msg.encode();
    assert_eq!(bytes[3], SUB_ID2_DISCOVERY_REPLY);
    match decoded(&bytes) {
        CiMessage::DiscoveryReply { to, data, .. } => {
            assert_eq!(to, Muid::of(DST));
            assert_eq!(data, sample_discovery());
        }
        other => panic!("expected Discovery reply, got {other:?}"),
    }
}

#[test]
fn categories_survive_round_trip() {
    let d = sample_discovery();
    let mut body = Vec::new();
    d.encode_body(true, &mut body);
    let back = DiscoveryData::decode_body(true, &body).expect("decodes");
    assert!(
        back.categories
            .contains(CiCategories::PROFILE_CONFIGURATION)
    );
    assert!(back.categories.contains(CiCategories::PROPERTY_EXCHANGE));
    assert!(!back.categories.contains(CiCategories::PROCESS_INQUIRY));
}

// Table 7: D2 Profile Configuration, D3 Property Exchange, D4 Process Inquiry;
// D1, Protocol Negotiation, is deprecated, so a 1.1 peer's D1 is not read.
#[test]
fn the_category_bits_are_table_7s_and_a_peers_deprecated_d1_is_dropped() {
    assert_eq!(CiCategories::PROFILE_CONFIGURATION.bits(), 0x04);
    assert_eq!(CiCategories::PROPERTY_EXCHANGE.bits(), 0x08);
    assert_eq!(CiCategories::PROCESS_INQUIRY.bits(), 0x10);
    let mut body = Vec::new();
    sample_discovery().encode_body(true, &mut body);
    body[11] = 0x02 | 0x04;
    let back = DiscoveryData::decode_body(true, &body).expect("decodes");
    assert_eq!(back.categories, CiCategories::PROFILE_CONFIGURATION);
}

// §5.5.3: the Receivable Maximum SysEx Message Size is 128 or greater.
#[test]
fn a_receivable_size_under_128_is_malformed() {
    let mut body = Vec::new();
    sample_discovery().encode_body(false, &mut body);
    let size = |body: &mut [u8], v: u32| {
        body[12..16].copy_from_slice(&[
            (v & 0x7F) as u8,
            ((v >> 7) & 0x7F) as u8,
            ((v >> 14) & 0x7F) as u8,
            ((v >> 21) & 0x7F) as u8,
        ]);
    };
    size(&mut body, 128);
    assert!(!is_malformed(&body, false));
    size(&mut body, 127);
    assert!(is_malformed(&body, false));
    size(&mut body, 0);
    assert!(is_malformed(&body, false));
}

// §5.6.2: the Function Block is the number of the Function Block, 0 to 31, or
// 0x7F for none; the numbers between are not a Function Block.
#[test]
fn the_function_block_of_a_reply_is_0_to_31_or_none() {
    let mut body = Vec::new();
    sample_discovery().encode_body(true, &mut body);
    let at = 16 + 1;
    for (raw, expected) in [(0, Some(0)), (31, Some(31)), (0x7F, None)] {
        body[at] = raw;
        let got = DiscoveryData::decode_body(true, &body).unwrap();
        assert_eq!(
            got.function_block.map(FunctionBlockId::get),
            expected,
            "{raw}"
        );
    }
    for raw in [32, 0x40, 0x7E] {
        body[at] = raw;
        assert!(is_malformed(&body, true), "{raw}");
    }
    // A Discovery has no such field: the byte is not read as one.
    body[at] = 0x40;
    assert!(!is_malformed(&body[..at], false));
}

#[test]
fn a_byte_with_its_top_bit_set_is_malformed() {
    let mut body = Vec::new();
    sample_discovery().encode_body(true, &mut body);
    for i in [0, 3, 8, 11, 16] {
        let mut bad = body.clone();
        bad[i] |= 0x80;
        assert!(is_malformed(&bad, true), "byte {i}");
    }
}

#[cfg(feature = "serde")]
#[test]
fn the_receivable_size_is_a_range_checked_value() {
    use tutti_fixtures::{RangeContract, check_range_contract};
    check_range_contract(&RangeContract {
        new: MaxSysexSize::new,
        get: MaxSysexSize::get,
        of: Some(MaxSysexSize::of),
        valid: &[128, 512, 0x0FFF_FFFF],
        invalid: &[0, 127, 0x1000_0000, u32::MAX],
    });
}

// M2-101 Table 6: after the 13-byte preamble come the manufacturer's three
// bytes, the family and the model (each LSB first), the four software
// revision bytes, the category bitmap and the four size bytes. The identity
// is `DeviceIdentity`'s arrays in that order.
#[test]
fn the_identity_is_written_in_the_tables_order() {
    let mut body = Vec::new();
    sample_discovery().encode_body(false, &mut body);
    assert_eq!(
        &body[..11],
        [0x00, 0x21, 0x09, 0x34, 0x24, 0x55, 0x00, 1, 2, 3, 4]
    );
}
