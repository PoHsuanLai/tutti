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
    // length, then that many text bytes. All of it must survive.
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
    assert!(
        back.categories
            .contains(CiCategories::PROFILE_CONFIGURATION)
    );
    assert!(back.categories.contains(CiCategories::PROPERTY_EXCHANGE));
    assert!(!back.categories.contains(CiCategories::PROCESS_INQUIRY));
}
