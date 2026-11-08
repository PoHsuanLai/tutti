use super::*;
use tutti_midi_types::midi2::flex_data::FlexData;
use tutti_ump::MidiChannel;

fn negotiator() -> EndpointNegotiator {
    EndpointNegotiator::new(
        DeviceIdentity {
            manufacturer: [0x00, 0x21, 0x09],
            family: 0x1234,
            family_model: 0x0001,
            software_version: [1, 0, 0, 0],
        },
        vec![FunctionBlock {
            block_number: 0,
            first_group: MidiGroup::FIRST,
            num_groups: 1,
            direction: FunctionBlockDirection::Bidirectional,
            name: "Keys".into(),
        }],
    )
    .with_names("Tutti", "tutti-0001")
}

#[test]
fn responds_to_full_discovery_with_all_notifications() {
    let n = negotiator();
    let replies = n.respond_to(&EndpointNegotiator::discovery_request());
    // info + identity + name + product-id + stream-config + one function block.
    assert_eq!(replies.len(), 6);
    assert!(matches!(
        UmpMessage::try_from(replies[0].data_words()).unwrap(),
        UmpMessage::UmpStream(UmpStream::EndpointInfo(_))
    ));
    assert!(matches!(
        UmpMessage::try_from(replies[1].data_words()).unwrap(),
        UmpMessage::UmpStream(UmpStream::DeviceIdentity(_))
    ));
    assert!(matches!(
        UmpMessage::try_from(replies[2].data_words()).unwrap(),
        UmpMessage::UmpStream(UmpStream::EndpointName(_))
    ));
    assert!(matches!(
        UmpMessage::try_from(replies[3].data_words()).unwrap(),
        UmpMessage::UmpStream(UmpStream::ProductInstanceId(_))
    ));
    assert!(matches!(
        UmpMessage::try_from(replies[4].data_words()).unwrap(),
        UmpMessage::UmpStream(UmpStream::StreamConfigurationNotification(_))
    ));
    assert!(matches!(
        UmpMessage::try_from(replies[5].data_words()).unwrap(),
        UmpMessage::UmpStream(UmpStream::FunctionBlockInfo(_))
    ));
}

#[test]
fn empty_name_omits_its_notification() {
    // Default (no with_names) → name/product-id replies are skipped.
    let n = EndpointNegotiator::new(
        DeviceIdentity {
            manufacturer: [0, 0, 0],
            family: 0,
            family_model: 0,
            software_version: [0; 4],
        },
        vec![],
    );
    let replies = n.respond_to(&EndpointNegotiator::discovery_request());
    // info + identity + stream-config, no name/product-id, no blocks.
    assert_eq!(replies.len(), 3);
    assert!(replies.iter().all(|e| !matches!(
        UmpMessage::try_from(e.data_words()).unwrap(),
        UmpMessage::UmpStream(UmpStream::EndpointName(_))
            | UmpMessage::UmpStream(UmpStream::ProductInstanceId(_))
    )));
}

#[test]
fn endpoint_info_reports_block_count_and_capabilities() {
    let n = negotiator();
    let info = n.endpoint_info();
    let UmpMessage::UmpStream(UmpStream::EndpointInfo(m)) =
        UmpMessage::try_from(info.data_words()).unwrap()
    else {
        panic!("expected EndpointInfo");
    };
    assert_eq!(u8::from(m.number_of_function_blocks()), 1);
    assert!(m.supports_midi2_protocol());
    assert!(m.supports_midi1_protocol());
}

#[test]
fn partial_discovery_only_returns_requested_plus_blocks() {
    let n = negotiator();
    // Ask for identity only.
    let req = MidiEvent::endpoint_discovery(1, 1, EndpointDiscoveryRequest::DEVICE_IDENTITY);
    let replies = n.respond_to(&req);
    // identity + the (always-announced) function block.
    assert_eq!(replies.len(), 2);
    assert!(matches!(
        UmpMessage::try_from(replies[0].data_words()).unwrap(),
        UmpMessage::UmpStream(UmpStream::DeviceIdentity(_))
    ));
}

/// Three blocks, only two of them named, for the discovery-filter tests.
fn multi_block_negotiator() -> EndpointNegotiator {
    let block = |n: u8, name: &str| FunctionBlock {
        block_number: n,
        first_group: MidiGroup::of(n),
        num_groups: 1,
        direction: FunctionBlockDirection::Bidirectional,
        name: name.into(),
    };
    EndpointNegotiator::new(
        DeviceIdentity {
            manufacturer: [0, 0, 0],
            family: 0,
            family_model: 0,
            software_version: [0; 4],
        },
        vec![block(0, "Keys"), block(1, "Drums"), block(2, "")],
    )
}

fn is_info(e: &MidiEvent) -> bool {
    matches!(
        UmpMessage::try_from(e.data_words()).unwrap(),
        UmpMessage::UmpStream(UmpStream::FunctionBlockInfo(_))
    )
}

fn is_name(e: &MidiEvent) -> bool {
    matches!(
        UmpMessage::try_from(e.data_words()).unwrap(),
        UmpMessage::UmpStream(UmpStream::FunctionBlockName(_))
    )
}

#[test]
fn function_block_discovery_replies_for_one_block() {
    let n = multi_block_negotiator();
    let req = MidiEvent::function_block_discovery(1, FunctionBlockDiscoveryRequest::INFO);
    let replies = n.respond_to_function_block_discovery(&req);
    assert_eq!(replies.len(), 1);
    assert!(is_info(&replies[0]));
    let UmpMessage::UmpStream(UmpStream::FunctionBlockInfo(m)) =
        UmpMessage::try_from(replies[0].data_words()).unwrap()
    else {
        panic!("expected FunctionBlockInfo");
    };
    assert_eq!(
        u8::from(m.function_block_number()),
        1,
        "only the asked-for block"
    );
}

#[test]
fn function_block_discovery_all_blocks_replies_per_block() {
    // §7.1.8: with the number set to 0xFF "the reply shall be one Function
    // Block Info Notification message per Function Block".
    let n = multi_block_negotiator();
    let req = MidiEvent::function_block_discovery(
        ALL_FUNCTION_BLOCKS,
        FunctionBlockDiscoveryRequest::INFO,
    );
    let replies = n.respond_to_function_block_discovery(&req);
    assert_eq!(replies.len(), 3, "one Info per block");
    assert!(replies.iter().all(is_info));
}

#[test]
fn each_filter_bit_produces_its_own_reply() {
    // §7.1.7: "Each bit set will result in an individual reply." Both bits
    // on the two named blocks → Info+Name each; block 2 is unnamed, so it
    // contributes Info only.
    let n = multi_block_negotiator();
    let req = MidiEvent::function_block_discovery(
        ALL_FUNCTION_BLOCKS,
        FunctionBlockDiscoveryRequest::INFO | FunctionBlockDiscoveryRequest::NAME,
    );
    let replies = n.respond_to_function_block_discovery(&req);
    assert_eq!(replies.len(), 5, "3 Info + 2 Name (block 2 is unnamed)");
    assert_eq!(replies.iter().filter(|e| is_info(e)).count(), 3);
    assert_eq!(replies.iter().filter(|e| is_name(e)).count(), 2);
    // Info precedes its own Name, so a reader sees topology before label.
    assert!(is_info(&replies[0]));
    assert!(is_name(&replies[1]));
}

#[test]
fn name_only_request_returns_no_info() {
    let n = multi_block_negotiator();
    let req = MidiEvent::function_block_discovery(0, FunctionBlockDiscoveryRequest::NAME);
    let replies = n.respond_to_function_block_discovery(&req);
    assert_eq!(replies.len(), 1);
    assert!(is_name(&replies[0]), "the 'i' bit was clear");
}

#[test]
fn unknown_block_number_replies_with_nothing() {
    let n = multi_block_negotiator();
    let req = MidiEvent::function_block_discovery(9, FunctionBlockDiscoveryRequest::INFO);
    assert!(n.respond_to_function_block_discovery(&req).is_empty());
}

#[test]
fn non_discovery_events_are_ignored() {
    let n = multi_block_negotiator();
    // An Endpoint Discovery is a UMP Stream message but not *this* one.
    let req = EndpointNegotiator::discovery_request();
    assert!(n.respond_to_function_block_discovery(&req).is_empty());
}

#[test]
fn discovered_block_names_reach_the_inquiry() {
    // A Function Block Discovery asking for both, looped back through the
    // discoverer, yields blocks that carry their labels — the whole point of
    // the Name notification.
    let n = multi_block_negotiator();
    let req = MidiEvent::function_block_discovery(
        ALL_FUNCTION_BLOCKS,
        FunctionBlockDiscoveryRequest::INFO | FunctionBlockDiscoveryRequest::NAME,
    );
    let mut inq = EndpointInquiry::new();
    for ev in n.respond_to_function_block_discovery(&req) {
        inq.ingest(&ev);
    }
    let blocks = &inq.discovered.function_blocks;
    assert_eq!(blocks.len(), 3, "no duplicates from Info+Name on one block");
    assert_eq!(blocks[0].name, "Keys");
    assert_eq!(blocks[1].name, "Drums");
    assert_eq!(blocks[2].name, "");
}

#[test]
fn a_name_arriving_before_its_info_is_not_lost() {
    // §7.1.7 permits a name-only request, so Name can arrive with no Info
    // before it — and a later Info for that block must not wipe the label.
    let mut inq = EndpointInquiry::new();
    let mut name_msg = Vec::new();
    tutti_midi_types::ump::function_block_name(1, "Drums", &mut name_msg);
    for ev in &name_msg {
        inq.ingest(ev);
    }
    assert_eq!(inq.discovered.function_blocks[0].name, "Drums");

    inq.ingest(&MidiEvent::function_block_info(
        true,
        1,
        MidiGroup::of(3),
        2,
        FunctionBlockDirection::Input,
    ));
    let blocks = &inq.discovered.function_blocks;
    assert_eq!(blocks.len(), 1, "Info matched the block the Name created");
    assert_eq!(blocks[0].name, "Drums", "the name survived the Info");
    assert_eq!(
        blocks[0].first_group,
        MidiGroup::of(3),
        "and topology filled in"
    );
    assert_eq!(blocks[0].direction, FunctionBlockDirection::Input);
}

#[test]
fn re_queried_block_info_replaces_rather_than_duplicates() {
    // §7.1.8: a non-static endpoint re-sends Info when a property changes.
    // Ingesting the same block twice must update it, not grow the list.
    let mut inq = EndpointInquiry::new();
    inq.ingest(&MidiEvent::function_block_info(
        true,
        0,
        MidiGroup::FIRST,
        1,
        FunctionBlockDirection::Input,
    ));
    inq.ingest(&MidiEvent::function_block_info(
        true,
        0,
        MidiGroup::of(5),
        2,
        FunctionBlockDirection::Output,
    ));
    let blocks = &inq.discovered.function_blocks;
    assert_eq!(blocks.len(), 1, "one block, updated in place");
    assert_eq!(blocks[0].first_group, MidiGroup::of(5));
    assert_eq!(blocks[0].direction, FunctionBlockDirection::Output);
}

#[test]
fn inquiry_reconstructs_responder_from_its_reply_stream() {
    // Full loopback: the responder's replies to a discovery, fed back into
    // the discoverer, reconstruct the responder's declared identity/topology.
    let n = negotiator();
    let replies = n.respond_to(&EndpointInquiry::request());

    let mut inquiry = EndpointInquiry::new();
    for r in &replies {
        inquiry.ingest(r);
    }
    let d = inquiry
        .result()
        .expect("Endpoint Info was in the reply stream");

    assert_eq!(d.ump_version, Some(UmpVersion::V1_1));
    assert!(
        d.capabilities
            .contains(EndpointCapabilities::MIDI2_PROTOCOL)
    );
    assert!(
        d.capabilities
            .contains(EndpointCapabilities::MIDI1_PROTOCOL)
    );
    assert_eq!(d.protocol, Some(Protocol::Midi2));
    assert_eq!(
        d.identity,
        Some(DeviceIdentity {
            manufacturer: [0x00, 0x21, 0x09],
            family: 0x1234,
            family_model: 0x0001,
            software_version: [1, 0, 0, 0],
        })
    );
    assert_eq!(d.name, "Tutti");
    assert_eq!(d.product_instance_id, "tutti-0001");
    assert_eq!(d.function_blocks.len(), 1);
    assert_eq!(d.function_blocks[0].block_number, 0);
    assert_eq!(
        d.function_blocks[0].direction,
        FunctionBlockDirection::Bidirectional
    );
}

#[test]
fn inquiry_has_no_result_until_endpoint_info_seen() {
    let mut inquiry = EndpointInquiry::new();
    // A device-identity reply alone isn't enough.
    inquiry.ingest(&MidiEvent::device_identity([1, 2, 3], 4, 5, [6, 7, 8, 9]));
    assert!(inquiry.result().is_none());
    // A non-UMP-Stream event is ignored.
    assert!(!inquiry.ingest(&MidiEvent::note_on(
        MidiGroup::FIRST,
        MidiChannel::FIRST,
        60,
        0x8000
    )));
}

#[test]
fn non_discovery_event_yields_no_reply() {
    let n = negotiator();
    assert!(
        n.respond_to(&MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            60,
            0x8000
        ))
        .is_empty()
    );
    // A Flex message isn't UMP Stream either.
    let ev = MidiEvent::flex_set_tempo(MidiGroup::FIRST, tutti_types::Bpm::of(120.0));
    assert!(matches!(
        UmpMessage::try_from(ev.data_words()).unwrap(),
        UmpMessage::FlexData(FlexData::SetTempo(_))
    ));
    assert!(n.respond_to(&ev).is_empty());
}
