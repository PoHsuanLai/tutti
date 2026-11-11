use super::super::*;
use super::*;

fn header() -> CiHeader {
    CiHeader::new(Muid::new(0x0123_4567).unwrap())
}

fn initiator() -> Muid {
    Muid::new(0x0007_7777).unwrap()
}

fn decoded(bytes: &[u8]) -> CiMessage {
    match CiMessage::decode(bytes) {
        Decoded::Message(message) => message,
        other => panic!("expected a message, got {other:?}"),
    }
}

fn is_malformed(body: &[u8], is_reply: bool) -> bool {
    DiscoveryData::decode_body(is_reply, body).is_none()
}

fn sample_discovery() -> DiscoveryData {
    DiscoveryData {
        manufacturer: [0x00, 0x21, 0x09],
        family: 0x1234,
        family_model: 0x0055,
        software_revision: [1, 2, 3, 4],
        categories: CiCategories::PROFILE_CONFIGURATION | CiCategories::PROPERTY_EXCHANGE,
        max_sysex_size: MaxSysexSize::of(512),
        output_path_id: U7::of(0x05),
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
    assert_eq!(bytes.len(), CiHeader::LEN + DiscoveryData::DISCOVERY_LEN);
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
        U7::of(0x05)
    );
}

#[test]
fn a_version_1_body_still_decodes() {
    // §5.3: a receiver must "process the fields, values, and bits defined in
    // the received version" when it is lower than its own. A v1 peer sends
    // the body with no Version-2 trailer at all.
    let d = sample_discovery();
    let mut body = Vec::new();
    d.encode_body(true, &mut body);
    body.truncate(DiscoveryData::BODY_LEN); // strip the v2 trailer

    let back = DiscoveryData::decode_body(true, &body).expect("v1 body decodes");
    assert_eq!(back.manufacturer, d.manufacturer);
    assert_eq!(back.max_sysex_size, d.max_sysex_size);
    assert_eq!(
        back.output_path_id,
        U7::of(0),
        "absent path id defaults to 0"
    );
    assert_eq!(
        back.function_block, None,
        "an absent Function Block must not read as block 0"
    );
}

#[test]
fn discovery_reply_keeps_is_reply_flag() {
    let msg = CiMessage::DiscoveryReply {
        header: header(),
        to: initiator(),
        data: sample_discovery(),
    };
    let bytes = msg.encode();
    assert_eq!(bytes[3], SUB_ID2_DISCOVERY_REPLY);
    match decoded(&bytes) {
        CiMessage::DiscoveryReply { to, data, .. } => {
            assert_eq!(to, initiator());
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

// §5.5.3: "The allowed values in the Receivable Maximum SysEx Message Size are
// 128 or greater."
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

// §5.6.2: the Function Block is "the number of the Function Block", 0 to 31, or
// "0x7F (no Function Block)"; the numbers between are not a Function Block.
#[test]
fn the_function_block_of_a_reply_is_0_to_31_or_none() {
    let mut body = Vec::new();
    sample_discovery().encode_body(true, &mut body);
    let at = DiscoveryData::BODY_LEN + 1;
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

// Mutations (run), each failing the named test:
// - `encode_body` pushing the Function Block for a Discovery too:
//   `discovery_and_reply_have_the_version_2_trailers`.
// - `decode_body` defaulting an absent Function Block to 0:
//   `a_version_1_body_still_decodes`.
// - `Nak::encode_body` writing the text length as one byte:
//   `nak_carries_its_version_2_details_and_text` (300 characters).
// - `CiMessage::encode` stamping `SUB_ID2_DISCOVERY` on a reply:
//   `discovery_reply_keeps_is_reply_flag`.
