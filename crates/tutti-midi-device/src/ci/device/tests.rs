use super::super::{DetailsData, InquiryTarget, ProfileChannels, ProfileId, SpecificData};
use super::super::{Unread, UnreadReason};
use super::*;
use tutti_ump::ManufacturerId;

fn muid(raw: u32) -> Muid {
    Muid::new(raw).expect("a MUID")
}

fn identity() -> DeviceIdentity {
    DeviceIdentity {
        manufacturer: ManufacturerId::from_sysex_bytes([U7::of(0), U7::of(0x21), U7::of(0x09)])
            .unwrap(),
        family: [U7::of(1), U7::of(2)],
        model: [U7::of(3), U7::of(4)],
        revision: [U7::of(5), U7::of(6), U7::of(7), U7::of(8)],
    }
}

const OURS: u32 = 0x0100_0001;
const PEER: u32 = 0x0200_0002;
const OTHER: u32 = 0x0300_0003;

/// A device whose entropy is `draws`, then a counter that never repeats.
fn device(
    draws: impl IntoIterator<Item = u32>,
) -> (CiDevice<impl FnMut() -> u32>, Vec<CiAction>, Instant) {
    let now = Instant::now();
    let mut queue = draws.into_iter();
    let mut counter = 0x0400_0000u32;
    let entropy = move || {
        queue.next().unwrap_or_else(|| {
            counter += 1;
            counter
        })
    };
    let (device, actions) = CiDevice::new(
        identity(),
        CiCategories::empty(),
        MaxSysexSize::of(512),
        entropy,
        now,
    );
    (device, actions, now)
}

fn header(source: u32) -> CiHeader {
    CiHeader::new(muid(source))
}

fn data(path: u8, block: Option<u8>) -> DiscoveryData {
    let mut data = DiscoveryData::new(identity(), CiCategories::empty(), MaxSysexSize::of(128));
    data.output_path_id = U7::of(path);
    data.function_block = block.map(|b| FunctionBlockId::new(b).unwrap());
    data
}

fn reply(from: u32, to: u32, block: Option<u8>) -> Decoded {
    Decoded::Message(CiMessage::DiscoveryReply {
        header: header(from),
        to: muid(to),
        data: data(0, block),
    })
}

fn discovery(from: u32, path: u8) -> Decoded {
    Decoded::Message(CiMessage::Discovery {
        header: header(from),
        data: data(path, None),
    })
}

fn invalidate(from: u32, target: u32) -> Decoded {
    Decoded::Message(CiMessage::InvalidateMuid {
        header: header(from),
        target: muid(target),
    })
}

fn sends(actions: &[CiAction]) -> Vec<&CiMessage> {
    actions
        .iter()
        .filter_map(|a| match a {
            CiAction::Send(m) => Some(m),
            _ => None,
        })
        .collect()
}

#[test]
fn a_new_device_sends_a_discovery_from_its_muid_to_everyone_on_output_path_0() {
    // §4.1, §5.5, §5.5.4.
    let (device, actions, now) = device([OURS]);
    assert_eq!(device.muid(), muid(OURS));
    let [CiMessage::Discovery { header, data }] = sends(&actions)[..] else {
        panic!("one Discovery, got {actions:?}");
    };
    assert_eq!(header.source, muid(OURS));
    assert_eq!(header.device_id, CiDeviceId::FunctionBlock);
    assert_eq!(data.output_path_id, U7::of(0));
    assert_eq!(data.max_sysex_size, MaxSysexSize::of(512));
    assert_eq!(
        sends(&actions)[0].destination(),
        Destination::Broadcast,
        "§3.3.3"
    );
    assert_eq!(device.deadline(), Some(now + DISCOVERY_WINDOW));
}

#[test]
fn the_discovery_window_is_three_seconds_and_closes_once() {
    // §5.5.5: "at least 3 seconds".
    let (mut device, _, now) = device([OURS]);
    assert_eq!(DISCOVERY_WINDOW, Duration::from_secs(3));
    assert!(
        device
            .on_timeout(now + DISCOVERY_WINDOW - Duration::from_millis(1))
            .is_empty()
    );
    assert_eq!(
        device.on_timeout(now + DISCOVERY_WINDOW),
        vec![CiAction::WindowClosed]
    );
    assert!(device.on_timeout(now + 2 * DISCOVERY_WINDOW).is_empty());
    assert_eq!(device.deadline(), None);
}

#[test]
fn a_reply_to_our_discovery_finds_the_peer() {
    let (mut device, _, now) = device([OURS]);
    let actions = device.on_message(&reply(PEER, OURS, None), now);
    assert_eq!(actions, vec![CiAction::Found(muid(PEER))]);
    let peer = device.peer(muid(PEER)).expect("found");
    assert_eq!(peer.function_block(), None);
    assert_eq!(peer.discovery().identity, identity());
    assert_eq!(peer.product_instance_id(), None);
}

#[test]
fn a_reply_to_another_muid_is_not_ours() {
    let (mut device, _, now) = device([OURS]);
    assert!(device.on_message(&reply(PEER, OTHER, None), now).is_empty());
    assert_eq!(device.peers().count(), 0);
}

#[test]
fn two_replies_with_one_muid_in_a_round_are_a_collision_invalidated_and_dropped() {
    // §5.9.2.
    let (mut device, _, now) = device([OURS]);
    device.on_message(&reply(PEER, OURS, None), now);
    let actions = device.on_message(&reply(PEER, OURS, None), now);
    let [CiMessage::InvalidateMuid { header, target }] = sends(&actions)[..] else {
        panic!("an Invalidate MUID, got {actions:?}");
    };
    assert_eq!((header.source, *target), (muid(OURS), muid(PEER)));
    assert!(actions.contains(&CiAction::Lost(muid(PEER))));
    assert!(device.peer(muid(PEER)).is_none());
    // A third one is still a collision, not a fresh find.
    let again = device.on_message(&reply(PEER, OURS, None), now);
    assert_eq!(sends(&again).len(), 1);
    assert!(device.peer(muid(PEER)).is_none());
}

#[test]
fn a_discovery_is_answered_to_its_sender_echoing_the_output_path_with_no_function_block() {
    // §4.1, §5.6, §5.6.1, §5.6.2.
    let (mut device, _, now) = device([OURS]);
    let actions = device.on_message(&discovery(PEER, 9), now);
    let [CiMessage::DiscoveryReply { header, to, data }] = sends(&actions)[..] else {
        panic!("a reply, got {actions:?}");
    };
    assert_eq!(header.source, muid(OURS));
    assert_eq!(*to, muid(PEER));
    assert_eq!(data.output_path_id, U7::of(9));
    assert_eq!(data.function_block, None);
    assert_eq!(header.device_id, CiDeviceId::FunctionBlock);
    assert_eq!(device.muid(), muid(OURS));
}

#[test]
fn a_discovery_with_our_muid_is_answered_by_option_b() {
    // §5.9.1 Option B: Invalidate the duplicate, change our MUID, discover again.
    let (mut device, _, now) = device([OURS, 0x0500_0005]);
    let later = now + Duration::from_secs(1);
    let actions = device.on_message(&discovery(OURS, 0), later);
    assert_eq!(device.muid(), muid(0x0500_0005));
    assert_eq!(
        actions[0],
        CiAction::MuidChanged {
            old: muid(OURS),
            new: muid(0x0500_0005)
        }
    );
    let sent = sends(&actions);
    let [
        CiMessage::InvalidateMuid { header, target },
        CiMessage::Discovery { header: dh, .. },
    ] = sent[..]
    else {
        panic!("an Invalidate then a Discovery, got {actions:?}");
    };
    assert_eq!(*target, muid(OURS));
    assert_eq!(header.source, muid(0x0500_0005));
    assert_eq!(dh.source, muid(0x0500_0005));
    assert!(
        !sent
            .iter()
            .any(|m| matches!(m, CiMessage::DiscoveryReply { .. })),
        "Option A's reply is not sent"
    );
    assert_eq!(device.deadline(), Some(later + DISCOVERY_WINDOW));
}

/// A Reply from our MUID to our MUID answers our Discovery from a device that
/// drew our MUID (§5.9.1): this device never answers itself.
#[test]
fn a_reply_from_our_muid_to_us_is_a_collision_on_us() {
    let (mut device, _, now) = device([OURS]);
    let actions = device.on_message(&reply(OURS, OURS, None), now);
    assert!(matches!(actions[0], CiAction::MuidChanged { old, .. } if old == muid(OURS)));
    assert_ne!(device.muid(), muid(OURS));
    assert_eq!(device.peers().count(), 0);
}

#[test]
fn an_invalidate_of_our_muid_ends_everything_changes_it_and_discovers_again_without_replying() {
    // §5.9: terminate transactions, new MUID, no reply, new Discovery.
    let (mut device, _, now) = device([OURS]);
    device.on_message(&reply(PEER, OURS, None), now);
    let actions = device.on_message(&invalidate(OTHER, OURS), now);
    assert!(
        matches!(actions[0], CiAction::MuidChanged { old, new } if old == muid(OURS) && new != old)
    );
    assert!(actions.contains(&CiAction::Lost(muid(PEER))));
    let sent = sends(&actions);
    assert_eq!(sent.len(), 1, "only the new Discovery: {actions:?}");
    assert!(
        matches!(sent[0], CiMessage::Discovery { header, .. } if header.source == device.muid())
    );
    assert_eq!(device.peers().count(), 0);
}

#[test]
fn an_invalidate_of_a_peer_discards_it_and_sends_nothing() {
    let (mut device, _, now) = device([OURS]);
    device.on_message(&reply(PEER, OURS, None), now);
    let actions = device.on_message(&invalidate(OTHER, PEER), now);
    assert_eq!(actions, vec![CiAction::Lost(muid(PEER))]);
    assert!(device.peer(muid(PEER)).is_none());
    // A MUID nobody here knows changes nothing.
    assert!(
        device
            .on_message(&invalidate(OTHER, 0x0600_0006), now)
            .is_empty()
    );
    assert_eq!(device.muid(), muid(OURS));
}

fn bytes_of(sub_id2: u8, source: u32, destination: [u8; 4]) -> Vec<u8> {
    let mut bytes = vec![0x7E, 0x7F, 0x0D, sub_id2, 0x02];
    bytes.extend(muid(source).to_bytes());
    bytes.extend(destination);
    bytes.extend([0, 0, 0, 0, 0]);
    bytes
}

#[test]
fn a_message_to_us_we_do_not_support_is_nakked_with_0x01_naming_it() {
    // §5.3, Appendix E: an Inquiry: Property Exchange Capabilities (0x30) is not supported.
    let (mut device, _, now) = device([OURS]);
    let decoded = CiMessage::decode(&bytes_of(0x30, PEER, muid(OURS).to_bytes()));
    let actions = device.on_message(&decoded, now);
    let [CiMessage::Nak { header, to, nak }] = sends(&actions)[..] else {
        panic!("a NAK, got {actions:?}");
    };
    assert_eq!(header.source, muid(OURS));
    assert_eq!(*to, muid(PEER));
    assert_eq!(nak.nak_sub_id2, U7::of(0x30));
    assert_eq!(nak.status_code, Nak::STATUS_MESSAGE_NOT_SUPPORTED);
}

#[test]
fn a_malformed_message_to_us_is_nakked_with_0x41() {
    let (mut device, _, now) = device([OURS]);
    // A Discovery reply cut short.
    let mut bytes = bytes_of(0x71, PEER, muid(OURS).to_bytes());
    bytes.truncate(13 + 3);
    let actions = device.on_message(&CiMessage::decode(&bytes), now);
    let [CiMessage::Nak { nak, .. }] = sends(&actions)[..] else {
        panic!("a NAK, got {actions:?}");
    };
    assert_eq!(nak.status_code, Nak::STATUS_MALFORMED);
}

#[test]
fn a_message_for_another_muid_or_for_everyone_is_never_nakked() {
    let (mut device, _, now) = device([OURS]);
    let to_other = CiMessage::decode(&bytes_of(0x30, PEER, muid(OTHER).to_bytes()));
    assert!(device.on_message(&to_other, now).is_empty());
    let to_all = CiMessage::decode(&bytes_of(0x30, PEER, [0x7F; 4]));
    assert!(device.on_message(&to_all, now).is_empty());
    assert!(device.on_message(&Decoded::NotCi, now).is_empty());
}

#[test]
fn an_ack_or_a_nak_is_never_nakked() {
    let (mut device, _, now) = device([OURS]);
    for sub in [0x7D, 0x7F] {
        // Too short to read as a body, so each decodes however it does: unread
        // or read, neither is answered.
        let decoded = CiMessage::decode(&bytes_of(sub, PEER, muid(OURS).to_bytes()));
        assert!(device.on_message(&decoded, now).is_empty(), "{sub:#x}");
    }
    let unread = Decoded::Unread(Unread {
        device_id: 0x7F,
        sub_id2: 0x7D,
        version: 2,
        source: Some(muid(PEER)),
        destination: Some(Destination::Muid(muid(OURS))),
        reason: UnreadReason::Malformed,
    });
    assert!(device.on_message(&unread, now).is_empty());
}

#[test]
fn a_peer_with_a_function_block_is_asked_once_for_its_product_instance_id() {
    // §5.7: Inquiry: Endpoint, status 0x00, to a Function Block.
    let (mut device, _, now) = device([OURS]);
    let actions = device.on_message(&reply(PEER, OURS, Some(3)), now);
    assert_eq!(actions[0], CiAction::Found(muid(PEER)));
    let [CiMessage::EndpointInquiry { header, to, status }] = sends(&actions)[..] else {
        panic!("an inquiry, got {actions:?}");
    };
    assert_eq!(
        (header.source, *to, *status),
        (muid(OURS), muid(PEER), EndpointStatus::ProductInstanceId)
    );
    // A peer with no Function Block has no Endpoint to ask about.
    let none = device.on_message(&reply(OTHER, OURS, None), now);
    assert!(sends(&none).is_empty());
}

fn id_reply(from: u32, to: u32, text: &str) -> Decoded {
    Decoded::Message(CiMessage::EndpointReply {
        header: header(from),
        to: muid(to),
        info: EndpointInfo::ProductInstanceId(
            CiInstanceId::new(ProductInstanceId::new(text).unwrap()).unwrap(),
        ),
    })
}

#[test]
fn the_endpoint_reply_sets_the_peers_product_instance_id() {
    let (mut device, _, now) = device([OURS]);
    device.on_message(&reply(PEER, OURS, Some(0)), now);
    let actions = device.on_message(&id_reply(PEER, OURS, "SN-0042"), now);
    assert_eq!(actions, vec![CiAction::Updated(muid(PEER))]);
    assert_eq!(
        device
            .peer(muid(PEER))
            .unwrap()
            .product_instance_id()
            .map(ProductInstanceId::as_str),
        Some("SN-0042")
    );
}

#[test]
fn an_endpoint_reply_nobody_asked_for_is_ignored() {
    let (mut device, _, now) = device([OURS]);
    // A peer with no Function Block was never asked.
    device.on_message(&reply(PEER, OURS, None), now);
    assert!(
        device
            .on_message(&id_reply(PEER, OURS, "x"), now)
            .is_empty()
    );
    // A stranger.
    assert!(
        device
            .on_message(&id_reply(OTHER, OURS, "x"), now)
            .is_empty()
    );
    assert_eq!(device.peer(muid(PEER)).unwrap().product_instance_id(), None);
}

#[test]
fn a_nak_of_the_endpoint_inquiry_leaves_the_id_unknown() {
    let (mut device, _, now) = device([OURS]);
    device.on_message(&reply(PEER, OURS, Some(0)), now);
    let nak = Decoded::Message(CiMessage::Nak {
        header: header(PEER),
        to: muid(OURS),
        nak: Nak::new(U7::of(SUB_ID2_ENDPOINT_INQUIRY), Nak::STATUS_NAK),
    });
    assert!(device.on_message(&nak, now).is_empty(), "never answered");
    // The inquiry is over: a reply after the NAK is not the answer to one.
    assert!(
        device
            .on_message(&id_reply(PEER, OURS, "late"), now)
            .is_empty()
    );
    assert_eq!(device.peer(muid(PEER)).unwrap().product_instance_id(), None);
}

#[test]
fn an_endpoint_inquiry_to_us_is_nakked_for_we_have_no_id_to_give() {
    // §5.8: "not able to provide the requested information ... NAK".
    let (mut device, _, now) = device([OURS]);
    let inquiry = |to| {
        Decoded::Message(CiMessage::EndpointInquiry {
            header: header(PEER),
            to: muid(to),
            status: EndpointStatus::ProductInstanceId,
        })
    };
    let actions = device.on_message(&inquiry(OURS), now);
    let [CiMessage::Nak { to, nak, .. }] = sends(&actions)[..] else {
        panic!("a NAK, got {actions:?}");
    };
    assert_eq!(*to, muid(PEER));
    assert_eq!(nak.nak_sub_id2, U7::of(0x72));
    assert_eq!(nak.status_code, Nak::STATUS_NAK);
    assert!(device.on_message(&inquiry(OTHER), now).is_empty());
}

#[test]
fn a_new_muid_is_never_the_old_one_a_peer_or_reserved() {
    // Draws: ours, then the same again, a reserved one, the broadcast value, the
    // peer's, and finally a good one.
    let (mut device, _, now) = device([
        OURS,
        OURS,
        0x0FFF_FF10,
        Muid::BROADCAST_RAW,
        PEER,
        0x0700_0007,
    ]);
    device.on_message(&reply(PEER, OURS, None), now);
    device.on_message(&invalidate(OTHER, OURS), now);
    assert_eq!(device.muid(), muid(0x0700_0007));
}

/// Appendix A.1.1: a Thru or merge loops this device's own Discovery back, under
/// its MUID and with the body it sent. That is an echo, not a collision: nothing
/// is sent, the MUID stays, and nothing is answered (it would answer itself).
#[test]
fn our_own_discovery_looped_back_is_an_echo_not_a_collision() {
    let (mut device, started, now) = device([OURS]);
    let [CiMessage::Discovery { .. }] = sends(&started)[..] else {
        panic!("a Discovery at start, got {started:?}");
    };
    let echo = Decoded::Message(sends(&started)[0].clone());
    let actions = device.on_message(&echo, now + Duration::from_millis(5));
    assert!(actions.is_empty(), "an echo is ignored, got {actions:?}");
    assert_eq!(device.muid(), muid(OURS));
}

/// The same echo of a Reply: this device's answer to a peer comes back addressed
/// to the peer, which is not a collision on us.
#[test]
fn our_own_reply_looped_back_is_an_echo_not_a_collision() {
    let (mut device, _, now) = device([OURS]);
    let answered = device.on_message(&discovery(PEER, 3), now);
    let [CiMessage::DiscoveryReply { .. }] = sends(&answered)[..] else {
        panic!("a Reply to the peer, got {answered:?}");
    };
    let echo = Decoded::Message(sends(&answered)[0].clone());
    let actions = device.on_message(&echo, now + Duration::from_millis(5));
    assert!(actions.is_empty(), "an echo is ignored, got {actions:?}");
    assert_eq!(device.muid(), muid(OURS));
}

fn piano() -> ProfileId {
    ProfileId::from_bytes([0x7E, 1, 2, 3, 4]).unwrap()
}

fn on(
    device_id: CiDeviceId,
    source: u32,
    to: u32,
    message: impl FnOnce(CiHeader, Muid) -> CiMessage,
) -> Decoded {
    let mut header = header(source);
    header.device_id = device_id;
    Decoded::Message(message(header, muid(to)))
}

#[test]
fn a_profile_inquiry_to_us_is_answered_on_its_address_with_both_lists_empty() {
    // §7.13: a device with no Profiles replies with 0 enabled and 0 disabled; M2-102
    // §2.4: the Function Block's Reply "with no Profiles reported" ends the replies.
    for address in [
        CiDeviceId::FunctionBlock,
        CiDeviceId::Group,
        CiDeviceId::Channel(tutti_ump::MidiChannel::of(5)),
    ] {
        let (mut device, _, now) = device([OURS]);
        let inquiry = on(address, PEER, OURS, |header, to| {
            CiMessage::ProfileInquiry { header, to }
        });
        let actions = device.on_message(&inquiry, now);
        let [
            CiMessage::ProfileReply {
                header,
                to,
                profiles,
            },
        ] = sends(&actions)[..]
        else {
            panic!("one Reply, got {actions:?}");
        };
        assert_eq!(header.device_id, address);
        assert_eq!(header.source, muid(OURS));
        assert_eq!(*to, muid(PEER));
        assert_eq!(*profiles, ProfileLists::none());
    }
}

#[test]
fn a_profile_inquiry_for_another_muid_is_not_answered() {
    let (mut device, _, now) = device([OURS]);
    let inquiry = on(CiDeviceId::FunctionBlock, PEER, OTHER, |header, to| {
        CiMessage::ProfileInquiry { header, to }
    });
    assert!(device.on_message(&inquiry, now).is_empty());
}

#[test]
fn a_set_on_set_off_or_details_inquiry_to_us_is_nakked_0x04_with_the_profile_id() {
    // M2-101 §5.11.3 (details are the Profile Id), M2-102 §2.4, §2.6, §2.7.
    let channel = CiDeviceId::Channel(tutti_ump::MidiChannel::of(2));
    let messages = [
        (
            0x22u8,
            on(channel, PEER, OURS, |header, to| CiMessage::SetProfileOn {
                header,
                to,
                profile: piano(),
                channels: ProfileChannels::of(1),
            }),
        ),
        (
            0x23,
            on(channel, PEER, OURS, |header, to| CiMessage::SetProfileOff {
                header,
                to,
                profile: piano(),
            }),
        ),
        (
            0x28,
            on(channel, PEER, OURS, |header, to| {
                CiMessage::ProfileDetailsInquiry {
                    header,
                    to,
                    profile: piano(),
                    target: InquiryTarget::NUMBER_OF_CHANNELS,
                }
            }),
        ),
    ];
    for (sub, message) in messages {
        let (mut device, _, now) = device([OURS]);
        let actions = device.on_message(&message, now);
        let [CiMessage::Nak { header, to, nak }] = sends(&actions)[..] else {
            panic!("a NAK, got {actions:?}");
        };
        assert_eq!(header.device_id, channel, "{sub:#x}");
        assert_eq!(*to, muid(PEER));
        assert_eq!(nak.nak_sub_id2, U7::of(sub));
        assert_eq!(nak.status_code, Nak::STATUS_PROFILE_NOT_SUPPORTED);
        assert_eq!(nak.details.map(U7::get), [0x7E, 1, 2, 3, 4]);
    }
}

#[test]
fn a_set_on_for_another_muid_is_not_nakked() {
    let (mut device, _, now) = device([OURS]);
    let set = on(CiDeviceId::Group, PEER, OTHER, |header, to| {
        CiMessage::SetProfileOn {
            header,
            to,
            profile: piano(),
            channels: ProfileChannels::BY_ADDRESS,
        }
    });
    assert!(device.on_message(&set, now).is_empty());
}

#[test]
fn what_an_initiator_reads_is_not_answered_by_the_management_core() {
    // Replies, reports, a Details Reply and Profile Specific Data are the
    // Initiator's; answering a reply would trade messages for ever.
    let (mut device, _, now) = device([OURS]);
    let messages = [
        on(CiDeviceId::FunctionBlock, PEER, OURS, |header, to| {
            CiMessage::ProfileReply {
                header,
                to,
                profiles: ProfileLists::none(),
            }
        }),
        Decoded::Message(CiMessage::ProfileEnabled {
            header: header(PEER),
            profile: piano(),
            channels: ProfileChannels::BY_ADDRESS,
        }),
        Decoded::Message(CiMessage::ProfileAdded {
            header: header(PEER),
            profile: piano(),
        }),
        on(CiDeviceId::Group, PEER, OURS, |header, to| {
            CiMessage::ProfileDetailsReply {
                header,
                to,
                profile: piano(),
                target: InquiryTarget::NUMBER_OF_CHANNELS,
                data: DetailsData::default(),
            }
        }),
        Decoded::Message(CiMessage::ProfileSpecificData {
            header: header(PEER),
            to: Destination::Muid(muid(OURS)),
            profile: piano(),
            data: SpecificData::default(),
        }),
    ];
    for message in messages {
        assert!(device.on_message(&message, now).is_empty(), "{message:?}");
    }
}
