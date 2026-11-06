use super::*;

fn identity(mfr: [u8; 3]) -> DiscoveryData {
    DiscoveryData::new(
        mfr,
        0x1234,
        0x0055,
        [1, 0, 0, 0],
        CiCategories::PROFILE_CONFIGURATION | CiCategories::PROPERTY_EXCHANGE,
        512,
    )
}

fn responder() -> CiResponder {
    CiResponder::new(Muid(0x0011_2233), identity([0x00, 0x21, 0x09]))
        .with_profiles(vec![
            (ProfileId([0x7E, 1, 2, 3, 4]), ProfileSwitch::Enabled),
            (ProfileId([0x7E, 5, 6, 7, 8]), ProfileSwitch::Disabled),
        ])
        .with_properties(vec![CiProperty {
            header: br#"{"resource":"DeviceInfo"}"#.to_vec(),
            body: br#"{"name":"Tutti"}"#.to_vec(),
        }])
}

#[test]
fn discovery_loopback_recovers_identity() {
    let resp = responder();
    let mut init = CiInitiator::new(Muid(0x0044_5566), identity([0x11, 0x22, 0x33]));

    // Initiator → responder → initiator.
    let replies = resp.respond_to(&init.discovery());
    assert_eq!(replies.len(), 1);
    let feedback = init.ingest(&replies[0]);
    assert!(feedback.is_empty(), "no collision, no feedback");

    let dev = init.discovered().expect("discovered the responder");
    assert_eq!(dev.muid, Muid(0x0011_2233));
    assert_eq!(dev.identity.manufacturer, [0x00, 0x21, 0x09]);
    assert!(dev.categories.contains(CiCategories::PROFILE_CONFIGURATION));
}

#[test]
fn reply_echoes_the_initiators_output_path_id() {
    // §5.6.1: "The Reply to Discovery shall return the same Output Path ID
    // provided in the originating Discovery Message." It names the
    // *initiator's* MIDI Out connection, so a responder that answered with
    // its own id would point at the wrong path.
    let resp = responder();
    let mut init_identity = identity([0x11, 0x22, 0x33]);
    init_identity.output_path_id = 0x42;
    let init = CiInitiator::new(Muid(0x0044_5566), init_identity);

    let replies = resp.respond_to(&init.discovery());
    match &replies[0] {
        CiMessage::Discovery {
            is_reply: true,
            data,
            ..
        } => {
            assert_eq!(data.output_path_id, 0x42, "echoed, not the responder's");
            // …while the rest of the reply is still the responder's identity.
            assert_eq!(data.manufacturer, [0x00, 0x21, 0x09]);
        }
        other => panic!("expected a Discovery reply, got {other:?}"),
    }
}

#[test]
fn profile_inquiry_loopback_lists_profiles() {
    let resp = responder();
    let mut init = CiInitiator::new(Muid(0x0044_5566), identity([1, 2, 3]));
    init.ingest(&resp.respond_to(&init.discovery())[0]);

    let inquiry = init.profile_inquiry().expect("peer known");
    let reply = resp.respond_to(&inquiry);
    assert_eq!(reply.len(), 1);
    // The reply lists both profiles (one enabled, one disabled).
    match &reply[0] {
        CiMessage::Profile {
            state: ProfileState::InquiryReply { enabled, disabled },
            ..
        } => {
            assert_eq!(enabled.len(), 1);
            assert_eq!(disabled.len(), 1);
        }
        other => panic!("expected InquiryReply, got {other:?}"),
    }
    init.ingest(&reply[0]);
    assert_eq!(init.discovered().unwrap().profiles.len(), 2);
}

#[test]
fn property_get_loopback_returns_body() {
    let resp = responder();
    let header = br#"{"resource":"DeviceInfo"}"#.to_vec();
    let get = CiMessage::Property {
        header: CiHeader {
            device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
            ci_version: CI_VERSION,
            source: Muid(0x1),
            destination: resp.muid(),
        },
        data: PropertyData {
            kind: PropertyKind::GetData,
            request_id: 3,
            header: header.clone(),
            num_chunks: 1,
            chunk: 1,
            body: Vec::new(),
        },
    };
    let reply = resp.respond_to(&get);
    match &reply[0] {
        CiMessage::Property { data, .. } => {
            assert_eq!(data.kind, PropertyKind::GetDataReply);
            assert_eq!(data.request_id, 3);
            assert_eq!(data.body, br#"{"name":"Tutti"}"#);
        }
        other => panic!("expected property reply, got {other:?}"),
    }
}

/// A Subscription message carrying `header`, addressed to `resp`.
fn subscription(resp: &CiResponder, header: &str) -> CiMessage {
    CiMessage::Property {
        header: CiHeader {
            device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
            ci_version: CI_VERSION,
            source: Muid(0x1),
            destination: resp.muid(),
        },
        data: PropertyData {
            kind: PropertyKind::Subscription,
            request_id: 9,
            header: header.as_bytes().to_vec(),
            num_chunks: 1,
            chunk: 1,
            body: Vec::new(),
        },
    }
}

#[test]
fn pe_capabilities_inquiry_is_answered() {
    // §8.4 has a peer ask this once before any other PE inquiry, so a NAK
    // here would stall the whole family before it starts.
    let resp = responder();
    let inquiry = CiMessage::PropertyCapabilities {
        header: CiHeader {
            device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
            ci_version: CI_VERSION,
            source: Muid(0x1),
            destination: resp.muid(),
        },
        is_reply: false,
        data: PropertyCapabilities {
            simultaneous_requests: 0,
            major_version: 0,
            minor_version: 0,
        },
    };
    let reply = resp.respond_to(&inquiry);
    assert_eq!(reply.len(), 1);
    match &reply[0] {
        CiMessage::PropertyCapabilities { is_reply, data, .. } => {
            assert!(*is_reply);
            assert_eq!(data.simultaneous_requests, 1, "the default we declare");
        }
        other => panic!("expected a capabilities reply, got {other:?}"),
    }
}

#[test]
fn a_capabilities_reply_is_observed_without_answering() {
    let resp = responder();
    let inbound = CiMessage::PropertyCapabilities {
        header: CiHeader {
            device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
            ci_version: CI_VERSION,
            source: Muid(0x1),
            destination: resp.muid(),
        },
        is_reply: true,
        data: PropertyCapabilities {
            simultaneous_requests: 4,
            major_version: 0,
            minor_version: 0,
        },
    };
    assert!(resp.respond_to(&inbound).is_empty());
}

#[test]
fn profile_details_inquiry_echoes_its_target() {
    // §7.7.1: the reply's target "shall be the same as in the Profile
    // Details Inquiry message which was received".
    let resp = responder();
    let profile = resp.profiles[0].0;
    let inquiry = CiMessage::Profile {
        header: CiHeader {
            device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
            ci_version: CI_VERSION,
            source: Muid(0x1),
            destination: resp.muid(),
        },
        state: ProfileState::DetailsInquiry {
            profile,
            target: 0x42,
        },
    };
    match &resp.respond_to(&inquiry)[0] {
        CiMessage::Profile {
            state: ProfileState::DetailsReply {
                target, profile: p, ..
            },
            ..
        } => {
            assert_eq!(*target, 0x42, "echoed, not re-chosen");
            assert_eq!(*p, profile);
        }
        other => panic!("expected a DetailsReply, got {other:?}"),
    }
}

#[test]
fn details_inquiry_for_an_unexposed_profile_is_nakked() {
    let resp = responder();
    let inquiry = CiMessage::Profile {
        header: CiHeader {
            device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
            ci_version: CI_VERSION,
            source: Muid(0x1),
            destination: resp.muid(),
        },
        state: ProfileState::DetailsInquiry {
            profile: ProfileId([0x63, 0x63, 0x63, 0x63, 0x63]),
            target: 0,
        },
    };
    assert!(matches!(
        &resp.respond_to(&inquiry)[0],
        CiMessage::Nak { .. }
    ));
}

#[test]
fn a_notify_is_honored_by_being_received_not_answered() {
    // §8.13 deprecates Notify in favour of ACK/NAK: devices "should not
    // send a Notify message" but "shall continue to honor the rules
    // receiving" one. Honoring it means accepting it — replying would emit
    // traffic the spec asks a device not to produce.
    let resp = responder();
    let inbound = CiMessage::Property {
        header: CiHeader {
            device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
            ci_version: CI_VERSION,
            source: Muid(0x1),
            destination: resp.muid(),
        },
        data: PropertyData {
            kind: PropertyKind::Notify,
            request_id: 1,
            header: br#"{"status":144}"#.to_vec(),
            num_chunks: 1,
            chunk: 1,
            body: Vec::new(),
        },
    };
    assert!(
        resp.respond_to(&inbound).is_empty(),
        "received without a reply, and without a NAK"
    );
}

#[test]
fn added_and_removed_reports_are_observed_without_answering() {
    // These are broadcast notifications (§7.4/§7.5). Answering a broadcast
    // would have every listening device reply at once.
    let resp = responder();
    for state in [
        ProfileState::Added(ProfileId([1, 2, 3, 4, 5])),
        ProfileState::Removed(ProfileId([1, 2, 3, 4, 5])),
    ] {
        let inbound = CiMessage::Profile {
            header: CiHeader {
                device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
                ci_version: CI_VERSION,
                source: Muid(0x1),
                destination: Muid::BROADCAST,
            },
            state,
        };
        assert!(resp.respond_to(&inbound).is_empty());
    }
}

#[test]
fn a_start_subscription_is_acknowledged_with_no_body() {
    // M2-103 §11.2 makes the reply mandatory; §11.1 says a Start's
    // "Response does not return any Property Data".
    let resp = responder();
    let reply = resp.respond_to(&subscription(
        &resp,
        r#"{"resource":"DeviceInfo","command":"start"}"#,
    ));
    assert_eq!(reply.len(), 1);
    match &reply[0] {
        CiMessage::Property { data, .. } => {
            assert_eq!(data.kind, PropertyKind::SubscriptionReply);
            assert_eq!(data.request_id, 9, "echoed so the initiator can match it");
            assert!(data.body.is_empty(), "a Start reply carries no data");
        }
        other => panic!("expected a SubscriptionReply, got {other:?}"),
    }
}

#[test]
fn an_end_subscription_is_acknowledged_without_naming_a_resource() {
    // §11.5 lets either side end a subscription, and an `end` is keyed on
    // subscribeId rather than a resource — so it must not be run through the
    // resource check that gates `start`.
    let resp = responder();
    let reply = resp.respond_to(&subscription(
        &resp,
        r#"{"subscribeId":"sub1","command":"end"}"#,
    ));
    assert_eq!(reply.len(), 1);
    assert!(matches!(
        &reply[0],
        CiMessage::Property {
            data: PropertyData {
                kind: PropertyKind::SubscriptionReply,
                ..
            },
            ..
        }
    ));
}

#[test]
fn a_start_for_an_unheld_resource_is_nakked() {
    // A peer cannot be subscribed to data this device does not hold. The resource, not
    // the message, is unsupported — so a NAK, not an empty reply.
    let resp = responder();
    let reply = resp.respond_to(&subscription(
        &resp,
        r#"{"resource":"NoSuch","command":"start"}"#,
    ));
    assert!(matches!(&reply[0], CiMessage::Nak { .. }));
}

#[test]
fn responder_only_commands_from_an_initiator_are_rejected() {
    // §11.1 marks partial/full/notify "Responder only". Inbound *to* the
    // responder they can only have come from an initiator talking out of
    // turn — the note under §8.11 says an Initiator "shall not send updates
    // to the Property Data by this message".
    let resp = responder();
    for cmd in ["partial", "full", "notify"] {
        let header = format!(r#"{{"resource":"DeviceInfo","command":"{cmd}"}}"#);
        let reply = resp.respond_to(&subscription(&resp, &header));
        assert!(
            matches!(&reply[0], CiMessage::Nak { .. }),
            "{cmd} is responder-only and must be rejected"
        );
    }
}

#[test]
fn a_subscription_reply_is_observed_without_answering() {
    // Replying to a reply would loop forever between two devices.
    let resp = responder();
    let inbound = CiMessage::Property {
        header: CiHeader {
            device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
            ci_version: CI_VERSION,
            source: Muid(0x1),
            destination: resp.muid(),
        },
        data: PropertyData {
            kind: PropertyKind::SubscriptionReply,
            request_id: 9,
            header: Vec::new(),
            num_chunks: 1,
            chunk: 1,
            body: Vec::new(),
        },
    };
    assert!(resp.respond_to(&inbound).is_empty());
}

#[test]
fn header_fields_are_read_out_of_realistic_json() {
    // The scanner is narrow by design, so pin what it must handle: key order
    // it does not control, whitespace, and a field that is absent.
    fn cmd(s: &str) -> Option<&str> {
        super::header_str_field(s.as_bytes(), "command")
    }
    assert_eq!(cmd(r#"{"command":"start"}"#), Some("start"));
    assert_eq!(cmd(r#"{ "command" : "end" }"#), Some("end"), "whitespace");
    assert_eq!(
        cmd(r#"{"resource":"X","command":"full"}"#),
        Some("full"),
        "not the first key"
    );
    assert_eq!(cmd(r#"{"resource":"X"}"#), None, "absent field");
    assert_eq!(cmd("not json at all"), None);
}

#[test]
fn unknown_property_is_nakked() {
    let resp = responder();
    let get = CiMessage::Property {
        header: CiHeader {
            device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
            ci_version: CI_VERSION,
            source: Muid(0x1),
            destination: resp.muid(),
        },
        data: PropertyData {
            kind: PropertyKind::GetData,
            request_id: 0,
            header: br#"{"resource":"Nope"}"#.to_vec(),
            num_chunks: 1,
            chunk: 1,
            body: Vec::new(),
        },
    };
    assert!(matches!(resp.respond_to(&get)[0], CiMessage::Nak { .. }));
}

#[test]
fn muid_collision_emits_invalidate() {
    let mut init = CiInitiator::new(Muid(0x0AAA_AAAA), identity([1, 2, 3]));
    // A reply that (wrongly) carries the initiator's own MUID as its source.
    let colliding = CiMessage::Discovery {
        header: CiHeader {
            device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
            ci_version: CI_VERSION,
            source: init.muid(),
            destination: init.muid(),
        },
        is_reply: true,
        data: identity([9, 9, 9]),
    };
    let feedback = init.ingest(&colliding);
    assert!(matches!(
        feedback.as_slice(),
        [CiMessage::InvalidateMuid { target, .. }] if *target == init.muid()
    ));
    assert!(init.discovered().is_none(), "collision is not a discovery");
}
