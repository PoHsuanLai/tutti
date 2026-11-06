use super::super::*;
use super::*;
use tutti_types::MidiGroup;

fn header() -> CiHeader {
    CiHeader {
        device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
        ci_version: CI_VERSION,
        source: Muid(0x10),
        destination: Muid(0x20),
    }
}

#[test]
fn get_and_set_round_trip_over_sysex7() {
    for kind in [
        PropertyKind::GetData,
        PropertyKind::GetDataReply,
        PropertyKind::SetData,
        PropertyKind::SetDataReply,
    ] {
        let data = PropertyData {
            kind,
            request_id: 7,
            header: br#"{"resource":"DeviceInfo"}"#.to_vec(),
            num_chunks: 1,
            chunk: 1,
            body: br#"{"name":"Tutti"}"#.to_vec(),
        };
        let m = CiMessage::Property {
            header: header(),
            data: data.clone(),
        };
        let mut events = Vec::new();
        ci_to_sysex7(MidiGroup::FIRST, &m, &mut events);
        let back = sysex7_to_ci(&events).expect("reassembles");
        assert_eq!(back, m);
    }
}

#[test]
fn subscription_round_trips_over_sysex7() {
    for kind in [PropertyKind::Subscription, PropertyKind::SubscriptionReply] {
        let data = PropertyData {
            kind,
            request_id: 3,
            header: br#"{"resource":"ChannelList","command":"start"}"#.to_vec(),
            num_chunks: 1,
            chunk: 1,
            body: Vec::new(),
        };
        let m = CiMessage::Property {
            header: header(),
            data: data.clone(),
        };
        let mut events = Vec::new();
        ci_to_sysex7(MidiGroup::FIRST, &m, &mut events);
        assert_eq!(sysex7_to_ci(&events).expect("reassembles"), m);
    }
}

#[test]
fn pe_capabilities_round_trip_both_directions() {
    for is_reply in [false, true] {
        let m = CiMessage::PropertyCapabilities {
            header: header(),
            is_reply,
            data: PropertyCapabilities {
                simultaneous_requests: 4,
                major_version: 0,
                minor_version: 0,
            },
        };
        assert_eq!(CiMessage::decode(&m.encode()).unwrap(), m);
        assert_eq!(m.encode()[3], if is_reply { 0x31 } else { 0x30 });
    }
}

#[test]
fn a_version_1_capabilities_body_still_decodes() {
    // §8.4 Table 30 marks the two version bytes "added in MIDI-CI Message
    // Version 2", so a v1.1 peer sends only the request count. §5.4 says a receiver
    // keep decoding it; Table 31 makes 0x00/0x00 the right reading of an
    // absent version rather than a guess.
    let mut bytes = CiMessage::PropertyCapabilities {
        header: header(),
        is_reply: false,
        data: PropertyCapabilities {
            simultaneous_requests: 2,
            major_version: 0,
            minor_version: 0,
        },
    }
    .encode();
    bytes.truncate(bytes.len() - 2);
    match CiMessage::decode(&bytes).expect("v1 body decodes") {
        CiMessage::PropertyCapabilities { data, .. } => {
            assert_eq!(data.simultaneous_requests, 2);
            assert_eq!(data.major_version, 0);
            assert_eq!(data.minor_version, 0);
        }
        other => panic!("expected PropertyCapabilities, got {other:?}"),
    }
}

#[test]
fn capabilities_do_not_decode_as_chunked_property_data() {
    // 0x30/0x31 sit inside the 0x30-0x3F Property Exchange range but carry
    // three scalars, not the chunked header/body. If `is_property_sub_id2`
    // claimed them, `PropertyData::decode_body` would read the request
    // count as a request id and then run off the end.
    assert!(!is_property_sub_id2(SUB_ID2_PE_CAPABILITIES));
    assert!(!is_property_sub_id2(SUB_ID2_PE_CAPABILITIES_REPLY));
    assert!(is_property_sub_id2(SUB_ID2_NOTIFY));
}

#[test]
fn notify_round_trips_with_the_chunked_body() {
    // Deprecated (§8.13) but must still be *received*, so the decode path
    // has to work even though nothing here emits one.
    let m = CiMessage::Property {
        header: header(),
        data: PropertyData {
            kind: PropertyKind::Notify,
            request_id: 5,
            header: br#"{"status":144}"#.to_vec(),
            num_chunks: 1,
            chunk: 1,
            body: Vec::new(),
        },
    };
    assert_eq!(CiMessage::decode(&m.encode()).unwrap(), m);
    assert_eq!(m.encode()[3], 0x3F);
}

#[test]
fn subscription_uses_the_spec_sub_ids() {
    // §8.11 / §8.12 fix these at 0x38 and 0x39. The wire body is identical
    // to Get/Set Property Data, so the sub-ID is the *only* thing that
    // distinguishes a subscription from an ordinary property exchange —
    // assert the byte rather than trusting a round-trip through this crate's own
    // encoder, which would agree with itself either way.
    let sub = PropertyData {
        kind: PropertyKind::Subscription,
        request_id: 0,
        header: Vec::new(),
        num_chunks: 1,
        chunk: 1,
        body: Vec::new(),
    };
    assert_eq!(sub.sub_id2(), 0x38);
    assert_eq!(
        PropertyData {
            kind: PropertyKind::SubscriptionReply,
            ..sub
        }
        .sub_id2(),
        0x39
    );
}

#[test]
fn subscription_commands_round_trip_through_their_wire_strings() {
    for cmd in [
        SubscriptionCommand::Start,
        SubscriptionCommand::Partial,
        SubscriptionCommand::Full,
        SubscriptionCommand::Notify,
        SubscriptionCommand::End,
    ] {
        assert_eq!(cmd.as_str().parse(), Ok(cmd));
    }
    assert_eq!(
        "subscribe".parse::<SubscriptionCommand>(),
        Err(UnknownSubscriptionCommand)
    );
    // The strings are the wire contract from M2-103 Table 39, so pin them.
    assert_eq!(SubscriptionCommand::Start.as_str(), "start");
    assert_eq!(SubscriptionCommand::Partial.as_str(), "partial");
    assert_eq!(SubscriptionCommand::Full.as_str(), "full");
    assert_eq!(SubscriptionCommand::Notify.as_str(), "notify");
    assert_eq!(SubscriptionCommand::End.as_str(), "end");
}

#[test]
fn each_command_is_restricted_to_its_direction() {
    // M2-103 §11.1 marks start "Initiator only" and partial/full/notify
    // "Responder only"; §11.5 lets either side send end. A device that
    // ignores this talks out of turn — a Responder cannot start its own
    // subscription, and an Initiator cannot push updates (the note under
    // §8.11: an Initiator "shall not send updates … but shall send updates
    // … using an Inquiry: Set Property Data message instead").
    let initiator = true;
    let responder = false;
    assert!(SubscriptionCommand::Start.is_valid_from(initiator));
    assert!(!SubscriptionCommand::Start.is_valid_from(responder));
    for cmd in [
        SubscriptionCommand::Partial,
        SubscriptionCommand::Full,
        SubscriptionCommand::Notify,
    ] {
        assert!(cmd.is_valid_from(responder), "{cmd:?} is responder-only");
        assert!(!cmd.is_valid_from(initiator), "{cmd:?} is responder-only");
    }
    assert!(SubscriptionCommand::End.is_valid_from(initiator));
    assert!(SubscriptionCommand::End.is_valid_from(responder));
}

#[test]
fn empty_header_and_body_round_trip() {
    let data = PropertyData {
        kind: PropertyKind::GetData,
        request_id: 0,
        header: Vec::new(),
        num_chunks: 1,
        chunk: 1,
        body: Vec::new(),
    };
    let m = CiMessage::Property {
        header: header(),
        data,
    };
    assert_eq!(CiMessage::decode(&m.encode()).unwrap(), m);
}
