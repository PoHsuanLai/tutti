use super::*;

#[test]
fn endpoint_discovery_decodes_via_midi2() {
    use midi2::UmpMessage;
    use midi2::ump_stream::UmpStream;
    let ev = endpoint_discovery(
        1,
        1,
        EndpointDiscoveryRequest::ENDPOINT_INFO | EndpointDiscoveryRequest::ENDPOINT_NAME,
    );
    match UmpMessage::try_from(ev.words()).unwrap() {
        UmpMessage::UmpStream(UmpStream::EndpointDiscovery(m)) => {
            assert_eq!(m.ump_version_major(), 1);
            assert_eq!(m.ump_version_minor(), 1);
            assert!(m.request_endpoint_info());
            assert!(!m.request_device_identity());
            assert!(m.request_endpoint_name());
        }
        other => panic!("expected EndpointDiscovery, got {other:?}"),
    }
}

#[test]
fn function_block_info_decodes_via_midi2() {
    use midi2::UmpMessage;
    use midi2::ump_stream::{Direction, UmpStream};
    let ev = function_block_info(true, 2, MidiGroup::of(4), 1, FunctionBlockDirection::Output);
    match UmpMessage::try_from(ev.words()).unwrap() {
        UmpMessage::UmpStream(UmpStream::FunctionBlockInfo(m)) => {
            assert!(m.active());
            assert_eq!(u8::from(m.function_block_number()), 2);
            assert_eq!(u8::from(m.first_group()), 4);
            assert_eq!(m.number_of_groups_spanned(), 1);
            assert_eq!(m.direction(), Direction::Output);
        }
        other => panic!("expected FunctionBlockInfo, got {other:?}"),
    }
}

#[test]
fn endpoint_info_decodes_via_midi2() {
    use midi2::UmpMessage;
    use midi2::ump_stream::UmpStream;
    let ev = endpoint_info(
        UmpVersion::V1_1,
        FunctionBlocks {
            count: 3,
            is_static: true,
        },
        EndpointCapabilities::MIDI2_PROTOCOL | EndpointCapabilities::MIDI1_PROTOCOL,
    );
    match UmpMessage::try_from(ev.words()).unwrap() {
        UmpMessage::UmpStream(UmpStream::EndpointInfo(m)) => {
            assert_eq!(m.ump_version_major(), 1);
            assert_eq!(u8::from(m.number_of_function_blocks()), 3);
            assert!(m.static_function_blocks());
            assert!(m.supports_midi2_protocol());
            assert!(m.supports_midi1_protocol());
            assert!(!m.supports_sending_jr_timestamps());
        }
        other => panic!("expected EndpointInfo, got {other:?}"),
    }
}

#[test]
fn device_identity_decodes_via_midi2() {
    use midi2::UmpMessage;
    use midi2::ump_stream::UmpStream;
    let ev = device_identity([0x00, 0x21, 0x09], 0x1234, 0x0001, [1, 2, 3, 4]);
    match UmpMessage::try_from(ev.words()).unwrap() {
        UmpMessage::UmpStream(UmpStream::DeviceIdentity(m)) => {
            assert_eq!(u8::from(m.device_manufacturer()[2]), 0x09);
            assert_eq!(u16::from(m.device_family()), 0x1234);
        }
        other => panic!("expected DeviceIdentity, got {other:?}"),
    }
}

#[test]
fn stream_configuration_decodes_via_midi2() {
    use midi2::UmpMessage;
    use midi2::ump_stream::UmpStream;
    let ev = stream_configuration_notification(
        Protocol::Midi2,
        JrTimestamps::SEND | JrTimestamps::RECEIVE,
    );
    match UmpMessage::try_from(ev.words()).unwrap() {
        UmpMessage::UmpStream(UmpStream::StreamConfigurationNotification(m)) => {
            assert_eq!(m.protocol(), 2);
            assert!(m.send_jr_timestamps());
            assert!(m.receive_jr_timestamps());
        }
        other => panic!("expected StreamConfigurationNotification, got {other:?}"),
    }
}

#[test]
fn stream_configuration_request_decodes_via_midi2() {
    use midi2::UmpMessage;
    use midi2::ump_stream::UmpStream;
    let ev = stream_configuration_request(Protocol::Midi1, JrTimestamps::RECEIVE);
    match UmpMessage::try_from(ev.words()).unwrap() {
        UmpMessage::UmpStream(UmpStream::StreamConfigurationRequest(m)) => {
            assert_eq!(m.protocol(), 1);
            assert!(m.receive_jr_timestamps());
            assert!(!m.send_jr_timestamps());
        }
        other => panic!("expected StreamConfigurationRequest, got {other:?}"),
    }
}

#[test]
fn stream_configuration_request_is_status_05_not_the_notification() {
    // The Request (0x05) and the Notification (0x06) carry identical
    // payloads and differ only in status. Asserting the status nibble by
    // hand is what keeps the two from being swapped: a round-trip through
    // midi2 would be equally happy with either, and sending a Notification
    // where §7.1.6.2 wants a Request asks a peer for nothing.
    let req = stream_configuration_request(Protocol::Midi2, JrTimestamps::empty());
    let note = stream_configuration_notification(Protocol::Midi2, JrTimestamps::empty());
    let status = |e: &Ump| (e.words()[0] >> 16) & 0x03FF;
    assert_eq!(status(&req), 0x05, "Request is status 0x05");
    assert_eq!(status(&note), 0x06, "Notification is status 0x06");
    assert_eq!(req.words()[0], 0xF005_0200);
}

#[test]
fn function_block_discovery_decodes_via_midi2() {
    use midi2::UmpMessage;
    use midi2::ump_stream::UmpStream;
    let ev = function_block_discovery(
        3,
        FunctionBlockDiscoveryRequest::INFO | FunctionBlockDiscoveryRequest::NAME,
    );
    match UmpMessage::try_from(ev.words()).unwrap() {
        UmpMessage::UmpStream(UmpStream::FunctionBlockDiscovery(m)) => {
            assert_eq!(m.function_block_number(), 3);
            assert!(m.requesting_function_block_info());
            assert!(m.requesting_function_block_name());
        }
        other => panic!("expected FunctionBlockDiscovery, got {other:?}"),
    }
}

#[test]
fn function_block_discovery_filter_bits_are_independent() {
    use midi2::UmpMessage;
    use midi2::ump_stream::UmpStream;
    // §7.1.7 Figure 21 places 'i' and 'n' in distinct bits, so asking for
    // one must not imply the other — the caller controls how many replies
    // it gets back.
    let decode = |ev: &Ump| match UmpMessage::try_from(ev.words()).unwrap() {
        UmpMessage::UmpStream(UmpStream::FunctionBlockDiscovery(m)) => (
            m.requesting_function_block_info(),
            m.requesting_function_block_name(),
        ),
        other => panic!("expected FunctionBlockDiscovery, got {other:?}"),
    };
    let info_only = function_block_discovery(0, FunctionBlockDiscoveryRequest::INFO);
    let name_only = function_block_discovery(0, FunctionBlockDiscoveryRequest::NAME);
    assert_eq!(decode(&info_only), (true, false));
    assert_eq!(decode(&name_only), (false, true));
}

#[test]
fn function_block_discovery_carries_the_all_blocks_sentinel() {
    use midi2::UmpMessage;
    use midi2::ump_stream::UmpStream;
    // §7.1.7: "Use 0xFF to request information about all Function Blocks."
    // Individual blocks are 0x00..=0x1F, so 0xFF must survive intact rather
    // than being masked into a block number the way a 7-bit field would.
    let ev = function_block_discovery(ALL_FUNCTION_BLOCKS, FunctionBlockDiscoveryRequest::INFO);
    match UmpMessage::try_from(ev.words()).unwrap() {
        UmpMessage::UmpStream(UmpStream::FunctionBlockDiscovery(m)) => {
            assert_eq!(m.function_block_number(), 0xFF);
        }
        other => panic!("expected FunctionBlockDiscovery, got {other:?}"),
    }
}

#[test]
fn function_block_name_round_trips() {
    use midi2::UmpMessage;
    use midi2::ump_stream::UmpStream;

    // Short name: one packet, decodes directly.
    let mut out = Vec::new();
    function_block_name(2, "Keys", &mut out);
    assert_eq!(out.len(), 1);
    match UmpMessage::try_from(out[0].words()).unwrap() {
        UmpMessage::UmpStream(UmpStream::FunctionBlockName(m)) => {
            assert_eq!(m.function_block(), 2);
            assert_eq!(m.name(), "Keys");
        }
        other => panic!("expected FunctionBlockName, got {other:?}"),
    }

    // A name too long for one packet fragments; reassembling the words
    // recovers it, so the block's label survives however it is split.
    let long = "Grand Piano — Upper Manual, Layered Strings";
    let mut out = Vec::new();
    function_block_name(7, long, &mut out);
    assert!(out.len() > 1, "long name spans packets");
    let words: Vec<u32> = out.iter().flat_map(|e| e.words().to_vec()).collect();
    match UmpMessage::try_from(&words[..]).unwrap() {
        UmpMessage::UmpStream(UmpStream::FunctionBlockName(m)) => {
            assert_eq!(m.function_block(), 7);
            assert_eq!(m.name(), long);
        }
        other => panic!("expected FunctionBlockName, got {other:?}"),
    }
}
