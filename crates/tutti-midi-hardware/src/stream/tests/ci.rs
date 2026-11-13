//! MIDI-CI on the wire (design 017 L8 part 3) against a fake device, through
//! the session and the fake backend: what is sent is the fake's sink, what the
//! device says is the input's producer.

use super::*;
use crate::{CiChange, Polled};
use tutti_midi_device::{
    CI_VERSION, CiCategories, CiDeviceId, CiHeader, CiMessage, Decoded, Destination, DiscoveryData,
    INQUIRY_TIMEOUT, MaxSysexSize, Muid,
};
use tutti_ump::{MidiGroup, U7};

const GROUP_0: MidiGroup = MidiGroup::of(0);

/// The MIDI-CI messages among what the fake device received, with their Group.
fn ci_sent(wire: &Wire) -> Vec<(MidiGroup, CiMessage)> {
    payloads(&received(wire))
        .into_iter()
        .filter_map(|(group, payload)| match CiMessage::decode(&payload) {
            Decoded::Message(message) => Some((group, message)),
            _ => None,
        })
        .collect()
}

/// The device says `message` on `group` at `ms`.
fn say(wire: &Wire, group: MidiGroup, message: &CiMessage, ms: u64) {
    reply_at(wire, message.packets(group).collect(), ms);
}

/// A peer of ours on the wire, and what it declares.
fn peer() -> Muid {
    Muid::new(0x0012_3456).unwrap()
}

fn peer_data() -> DiscoveryData {
    DiscoveryData::new(identity(), CiCategories::empty(), MaxSysexSize::of(256))
}

fn polled(wire: &Wire, ms: u64) -> Vec<Polled> {
    wire.session.poll(wire.at(ms)).collect()
}

/// A block of one Group at `first_group` that declares MIDI-CI `version`.
fn ci_block(id: u8, first_group: u8, version: u8) -> FunctionBlock {
    FunctionBlock::builder(
        FunctionBlockId::new(id).unwrap(),
        BlockDirection::Bidirectional,
        FirstGroup::new(first_group).unwrap(),
        GroupSpan::new(1).unwrap(),
    )
    .midi_ci(version, 0)
    .name(BlockName::new("B").unwrap())
    .build()
    .unwrap()
}

fn endpoint_of(blocks: &[FunctionBlock]) -> Endpoint {
    let device = Endpoint::new(Capabilities {
        midi1: true,
        midi2: true,
        ..Capabilities::default()
    });
    blocks
        .iter()
        .cloned()
        .fold(device, Endpoint::with_function_block)
}

/// A negotiated UMP Endpoint with `blocks`, and the poll after the settle,
/// which starts MIDI-CI.
fn endpoint_wire(blocks: &[FunctionBlock]) -> Wire {
    let mut wire = session_over(
        UmpCapability::midi2(),
        PortDirection::Duplex,
        endpoint_of(blocks),
    );
    wire.session.enable_ci(identity());
    wire.session.connect_input(&wire.id.clone()).unwrap();
    // One Function Block Discovery per declared block takes a few ticks.
    let settled = (0..32).any(|ms| !wire.tick(ms).1.is_empty());
    assert!(settled, "the negotiation settled");
    assert!(
        payloads(&received(&wire)).is_empty(),
        "the poll that settles starts nothing: the port is told to listen first"
    );
    let _ = polled(&wire, 100);
    wire
}

/// A MIDI 1.0 port whose Identity Request was answered and whose MIDI-CI has
/// started, with the MUID it drew; what was sent so far is read off.
fn midi1_running() -> (Wire, Muid) {
    let wire = midi1_wire();
    wire.session.enable_ci(identity());
    let _ = polled(&wire, 0);
    reply_at(&wire, identity_reply(0), 10);
    let _ = polled(&wire, 20);
    let _ = received(&wire);
    let _ = polled(&wire, 30);
    let sent = ci_sent(&wire);
    let [(group, CiMessage::Discovery { header, .. })] = sent.as_slice() else {
        panic!("one Discovery expected");
    };
    assert_eq!(*group, GROUP_0);
    let source = header.source;
    (wire, source)
}

/// M2-101 §3.2.2 and Table 6: a UMP Endpoint's Function Block that declares
/// MIDI-CI is a MIDI-CI Device of its own, on the block's first Group, and
/// opens with a Discovery to the Broadcast MUID from the Function Block's
/// Device ID (`7F`): the bytes of Table 6. A block that declares none (M2-104
/// §7.1.8) is sent nothing.
#[test]
fn a_function_block_that_declares_midi_ci_is_sent_discovery_on_its_first_group() {
    let wire = endpoint_wire(&[ci_block(0, 2, 2), ci_block(1, 4, 0)]);
    let sent = payloads(&received(&wire));
    let [(group, payload)] = sent.as_slice() else {
        panic!("one Discovery, on the block that declares MIDI-CI: {sent:?}");
    };
    assert_eq!(*group, MidiGroup::of(2), "the block's first Group");
    // F0 7E <device ID> 0D 70 <version> <source MUID> <destination MUID> ...
    assert_eq!(payload[..4], [0x7E, 0x7F, 0x0D, 0x70]);
    assert_eq!(payload[4], CI_VERSION.get());
    assert_eq!(payload[9..13], [0x7F; 4], "the Broadcast MUID");
    assert_eq!(payload.len(), CiHeader::LEN + DiscoveryData::DISCOVERY_LEN);
    let Decoded::Message(CiMessage::Discovery { header, data }) = CiMessage::decode(payload) else {
        panic!("a Discovery");
    };
    assert_eq!(header.device_id, CiDeviceId::FunctionBlock);
    // The body (Table 6): exactly the app's identity (`identity()`: manufacturer
    // `41 00 00`, family 1 2, model 3 4, revision 5 6 7 8), no categories, 512 as
    // four 7-bit bytes, Output Path 0.
    assert_eq!(payload[13..16], [0x41, 0x00, 0x00]);
    assert_eq!(payload[16..24], [1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!(data.identity, identity());
    assert_eq!(payload[24], 0);
    assert_eq!(payload[25..29], [0x00, 0x04, 0x00, 0x00]);
    assert_eq!(payload[29], 0);
    assert_eq!(data.max_sysex_size, MaxSysexSize::of(512));
}

/// An Endpoint whose blocks all declare no MIDI-CI runs none: nothing is sent
/// however long it goes on, and the port does not copy its SysEx.
#[test]
fn an_endpoint_whose_blocks_declare_no_midi_ci_runs_none() {
    let wire = endpoint_wire(&[ci_block(0, 0, 0), ci_block(1, 3, 0)]);
    let slot = wire.session.ports().list_input_ports()[0].id;
    for ms in [200, 5_000, 20_000] {
        let _ = polled(&wire, ms);
    }
    assert!(received(&wire).is_empty());
    reply_at(&wire, sysex(&[0x7E, 0x7F, 0x09, 0x01]), 20_001);
    assert_eq!(wire.session.ports().drain_stream(slot).count(), 0);
}

/// §3.2.2: with no Function Block to say otherwise, the Endpoint's Group 0 is
/// one MIDI-CI Device. That holds for an Endpoint that answered with no blocks
/// and for one that never answered the Stream negotiation.
#[test]
fn an_endpoint_with_no_blocks_or_no_answer_gets_discovery_on_group_0() {
    // Answered, no blocks.
    let wire = endpoint_wire(&[]);
    let sent = payloads(&received(&wire));
    assert!(
        matches!(sent.as_slice(), [(g, _)] if *g == GROUP_0),
        "{sent:?}"
    );

    // Never answered: the Stream Discoveries are sent and dropped.
    let wire = session_over(
        UmpCapability::midi2(),
        PortDirection::Duplex,
        Endpoint::new(Capabilities::default()),
    );
    wire.session.enable_ci(identity());
    wire.session.connect_input(&wire.id.clone()).unwrap();
    let step = STEP_TIMEOUT.as_millis() as u64;
    for ms in [0, step, 2 * step, 3 * step + 1, 3 * step + 2] {
        let _ = polled(&wire, ms);
    }
    let sent = payloads(&received(&wire));
    assert!(
        matches!(sent.as_slice(), [(g, _)] if *g == GROUP_0),
        "{sent:?}"
    );
}

/// M2-101 routes by Group: a Discovery on the Group of a block is answered, one
/// on a Group with no MIDI-CI Device is not.
#[test]
fn messages_are_routed_by_group() {
    let wire = endpoint_wire(&[ci_block(0, 2, 2)]);
    let _ = received(&wire);
    let discovery = CiMessage::Discovery {
        header: CiHeader::new(peer()),
        data: peer_data(),
    };
    say(&wire, MidiGroup::of(5), &discovery, 200);
    let _ = polled(&wire, 210);
    assert!(ci_sent(&wire).is_empty(), "no device on Group 5");
    say(&wire, MidiGroup::of(2), &discovery, 220);
    let _ = polled(&wire, 230);
    let sent = ci_sent(&wire);
    assert!(
        matches!(sent.as_slice(), [(g, CiMessage::DiscoveryReply { .. })] if *g == MidiGroup::of(2)),
        "{sent:?}"
    );
}

/// A MIDI 1.0 port is asked its identity first, and MIDI-CI starts only when
/// that has ended, answered or given up, so the two never read the SysEx
/// stream at once: every Identity Request precedes the Discovery.
#[test]
fn a_midi1_port_is_sent_discovery_only_after_the_identity_inquiry_ended() {
    let ms = INQUIRY_TIMEOUT.as_millis() as u64;
    for answered in [true, false] {
        let wire = midi1_wire();
        wire.session.enable_ci(identity());
        let mut order = String::new();
        for step in 0..8 {
            if answered && step == 1 {
                reply_at(&wire, identity_reply(0), step * ms - 1);
            }
            let _ = polled(&wire, step * ms);
            for (_, payload) in payloads(&received(&wire)) {
                match CiMessage::decode(&payload) {
                    Decoded::Message(CiMessage::Discovery { .. }) => order.push('D'),
                    _ if payload == [0x7E, 0x7F, 0x06, 0x01] => order.push('R'),
                    other => panic!("sent {other:?}"),
                }
            }
        }
        assert_eq!(
            order,
            if answered { "RD" } else { "RRD" },
            "answered: {answered}"
        );
    }
}

/// A Reply to Discovery addressed to the device makes its sender a peer
/// (§5.6), found in the session's read and reported once by `poll`.
#[test]
fn a_reply_to_discovery_makes_the_peer_appear_and_is_reported_once() {
    let (wire, ours) = midi1_running();
    say(
        &wire,
        GROUP_0,
        &CiMessage::DiscoveryReply {
            header: CiHeader::new(peer()),
            to: ours,
            data: peer_data(),
        },
        40,
    );
    let found = polled(&wire, 50);
    let [Polled::Ci(id, CiChange::Found(info))] = found.as_slice() else {
        panic!("one peer found, got {found:?}");
    };
    assert_eq!(*id, wire.id);
    assert_eq!((info.group(), info.muid()), (GROUP_0, peer()));
    assert_eq!(info.peer().discovery(), &peer_data());
    assert_eq!(wire.session.ci_peers(&wire.id), std::slice::from_ref(info));
    assert!(polled(&wire, 60).is_empty(), "reported once");
}

/// Appendix E: a Responder answers Discovery from another device at any time,
/// long after its own window closed, with a Reply to that device that echoes
/// the Output Path ID (§5.6.1) and declares no Function Block (§5.6.2). A
/// Discovery is not a peer found.
#[test]
fn a_discovery_a_peer_sends_is_answered_long_after_the_window_closed() {
    let (wire, ours) = midi1_running();
    let _ = polled(&wire, 10_000);
    let mut data = peer_data();
    data.output_path_id = U7::of(5);
    say(
        &wire,
        GROUP_0,
        &CiMessage::Discovery {
            header: CiHeader::new(peer()),
            data,
        },
        10_001,
    );
    let none = polled(&wire, 10_002);
    assert!(none.is_empty(), "a Discovery finds no peer: {none:?}");
    let sent = ci_sent(&wire);
    let [(group, CiMessage::DiscoveryReply { header, to, data })] = sent.as_slice() else {
        panic!("one Reply to Discovery, got {sent:?}");
    };
    assert_eq!(*group, GROUP_0);
    assert_eq!((header.source, *to), (ours, peer()));
    assert_eq!(data.output_path_id, U7::of(5));
    assert_eq!(data.function_block, None);
    assert_eq!(data.identity, identity());
    assert!(wire.session.ci_peers(&wire.id).is_empty());
}

/// §5.9: an Invalidate MUID naming a found peer discards it, and `poll` reports
/// the loss.
#[test]
fn an_invalidate_muid_of_a_found_peer_removes_it_and_is_reported() {
    let (wire, ours) = midi1_running();
    say(
        &wire,
        GROUP_0,
        &CiMessage::DiscoveryReply {
            header: CiHeader::new(peer()),
            to: ours,
            data: peer_data(),
        },
        40,
    );
    let _ = polled(&wire, 50);
    assert_eq!(wire.session.ci_peers(&wire.id).len(), 1);
    say(
        &wire,
        GROUP_0,
        &CiMessage::InvalidateMuid {
            header: CiHeader::new(Muid::new(0x00AB_CDEF).unwrap()),
            target: peer(),
        },
        60,
    );
    let lost = polled(&wire, 70);
    assert!(
        matches!(
            lost.as_slice(),
            [Polled::Ci(_, CiChange::Lost { group, muid })] if *group == GROUP_0 && *muid == peer()
        ),
        "{lost:?}"
    );
    assert!(wire.session.ci_peers(&wire.id).is_empty());
}

/// A SysEx that is not MIDI-CI (General MIDI System On, M1 p.52) is ignored:
/// nothing is answered and no peer appears.
#[test]
fn a_sysex_that_is_not_midi_ci_is_ignored() {
    let (wire, _) = midi1_running();
    reply_at(&wire, sysex(&[0x7E, 0x7F, 0x09, 0x01]), 40);
    assert!(polled(&wire, 50).is_empty());
    assert!(received(&wire).is_empty());
    assert!(wire.session.ci_peers(&wire.id).is_empty());
}

/// What MIDI-CI sends goes out on the input's own output and never the
/// session's MIDI output, and the SysEx it hears still reaches the graph: the
/// port copies it, it does not divert it.
#[test]
fn nothing_midi_ci_reaches_the_sessions_output_and_the_graph_still_hears_sysex() {
    use crate::testing::Rig;
    let wire = midi1_wire();
    wire.session.enable_ci(identity());
    let other = EndpointId::from_raw(8);
    wire.counters.plug(
        crate::test_support::endpoint(8, "Other"),
        PortDirection::Output,
    );
    wire.session.connect_output("other").unwrap();

    let mut rig = Rig::over(wire.session.ports().inlets(), wire.start);
    rig.run(1);
    rig.apply();

    let _ = polled(&wire, 0);
    reply_at(&wire, identity_reply(0), 1);
    let _ = polled(&wire, 2);
    let _ = polled(&wire, 3);
    let sent = ci_sent(&wire);
    let [(_, CiMessage::Discovery { header, .. })] = sent.as_slice() else {
        panic!("the Discovery reached the device, got {sent:?}");
    };
    assert!(
        wire.counters.take_sent(other).is_empty(),
        "and not the session's output"
    );
    let reply = CiMessage::DiscoveryReply {
        header: CiHeader::new(peer()),
        to: header.source,
        data: peer_data(),
    };
    say(&wire, GROUP_0, &reply, 5);
    let found = polled(&wire, 6);
    assert!(matches!(
        found.as_slice(),
        [Polled::Ci(_, CiChange::Found(_))]
    ));
    assert!(wire.counters.take_sent(other).is_empty());

    rig.run(40);
    let heard: Vec<_> = rig.take().into_iter().map(|(_, _, ump)| ump).collect();
    let said: Vec<_> = [identity_reply(0), reply.packets(GROUP_0).collect()].concat();
    assert_eq!(heard, said, "the graph hears every packet MIDI-CI heard");
}

/// MIDI-CI "closes with the input": once it is disconnected its peers are
/// gone and nothing more is sent, whatever the poll's time.
#[test]
fn the_inputs_disconnect_ends_midi_ci() {
    let (wire, ours) = midi1_running();
    say(
        &wire,
        GROUP_0,
        &CiMessage::DiscoveryReply {
            header: CiHeader::new(peer()),
            to: ours,
            data: peer_data(),
        },
        40,
    );
    let _ = polled(&wire, 50);
    assert_eq!(wire.session.ci_peers(&wire.id).len(), 1, "alive");

    assert!(wire.session.disconnect_input(&wire.id.clone()));
    for ms in [60, 5_000, 60_000] {
        let _ = polled(&wire, ms);
    }
    assert!(received(&wire).is_empty());
    assert!(wire.session.ci_peers(&wire.id).is_empty());
}

/// A Destination the Reply to Discovery is for is one device, never Broadcast
/// (§3.3.3), as sent.
#[test]
fn a_reply_to_discovery_is_addressed_to_one_device() {
    let (wire, _) = midi1_running();
    say(
        &wire,
        GROUP_0,
        &CiMessage::Discovery {
            header: CiHeader::new(peer()),
            data: peer_data(),
        },
        40,
    );
    let _ = polled(&wire, 50);
    let sent = ci_sent(&wire);
    assert_eq!(
        sent.first().map(|(_, m)| m.destination()),
        Some(Destination::Muid(peer()))
    );
}

/// Once the inquiry has given up on a silent device, an enabled MIDI-CI takes over the
/// port's copy of the device's SysEx7 packets (it stays on, and the graph still
/// hears them, the next test), and no Identity Request is sent again: what
/// goes out is MIDI-CI's.
#[test]
fn after_giving_up_the_sysex_copy_stays_on_for_midi_ci_and_no_request_is_sent() {
    use tutti_midi_device::INQUIRY_ATTEMPTS;
    let wire = midi1_wire();
    wire.session.enable_ci(identity());
    let ms = INQUIRY_TIMEOUT.as_millis() as u64;
    // The request, its one retry, then the poll that gives up, then the one
    // that starts MIDI-CI.
    for step in 0..=u64::from(INQUIRY_ATTEMPTS) + 1 {
        assert!(wire.session.poll(wire.at(step * ms)).next().is_none());
    }
    let slot = wire.session.ports().list_input_ports()[0].id;
    let _ = received(&wire);
    // Whatever the ring holds from the inquiry is read off.
    let _ = wire.session.ports().drain_stream(slot).count();

    reply_at(&wire, identity_reply(0), 10 * ms);
    assert_eq!(
        wire.session.ports().drain_stream(slot).count(),
        identity_reply(0).len(),
        "the copy is on while MIDI-CI runs"
    );
    for step in 10..13 {
        let _ = wire.session.poll(wire.at(step * ms)).count();
    }
    assert_eq!(identity_requests(&received(&wire)), 0);
}

/// MIDI-CI is off until the app gives an identity: a settled block that declares
/// MIDI-CI and a MIDI 1.0 port are sent no Discovery, and the port's SysEx copy
/// is off after the Identity Request (which is sent all the same).
#[test]
fn midi_ci_is_off_by_default() {
    let mut wire = session_over(
        UmpCapability::midi2(),
        PortDirection::Duplex,
        endpoint_of(&[ci_block(0, 2, 2)]),
    );
    wire.session.connect_input(&wire.id.clone()).unwrap();
    let settled = (0..32).any(|ms| !wire.tick(ms).1.is_empty());
    assert!(settled);
    for ms in [100, 5_000, 20_000] {
        let _ = polled(&wire, ms);
    }
    assert!(
        received(&wire).is_empty(),
        "a block that declares MIDI-CI, unasked"
    );
    assert_eq!(wire.session.ci_identity(), None);

    use tutti_midi_device::{INQUIRY_ATTEMPTS, INQUIRY_TIMEOUT};
    let wire = midi1_wire();
    let ms = INQUIRY_TIMEOUT.as_millis() as u64;
    let mut requests = 0;
    for step in 0..=u64::from(INQUIRY_ATTEMPTS) + 2 {
        let _ = polled(&wire, step * ms);
        let sent = received(&wire);
        requests += identity_requests(&sent);
        assert_eq!(
            payloads(&sent).len(),
            identity_requests(&sent),
            "nothing but requests"
        );
    }
    assert_eq!(requests, usize::from(INQUIRY_ATTEMPTS));
    let slot = wire.session.ports().list_input_ports()[0].id;
    let _ = wire.session.ports().drain_stream(slot).count();
    reply_at(&wire, sysex(&[0x7E, 0x7F, 0x09, 0x01]), 20 * ms);
    assert_eq!(
        wire.session.ports().drain_stream(slot).count(),
        0,
        "the copy is off"
    );
}

/// Enabling reaches an input already open and says exactly the app's identity;
/// disabling sends each device's Invalidate MUID for its own MUID (§5.9),
/// reports its peers lost, stops the copy and answers nothing after.
#[test]
fn disabling_ends_midi_ci_with_an_invalidate_muid_and_the_loss_of_its_peers() {
    let (wire, ours) = midi1_running();
    assert_eq!(wire.session.ci_identity(), Some(identity()));
    say(
        &wire,
        GROUP_0,
        &CiMessage::DiscoveryReply {
            header: CiHeader::new(peer()),
            to: ours,
            data: peer_data(),
        },
        40,
    );
    let _ = polled(&wire, 50);
    assert_eq!(wire.session.ci_peers(&wire.id).len(), 1);

    wire.session.disable_ci();
    let ended = polled(&wire, 60);
    assert!(
        matches!(
            ended.as_slice(),
            [Polled::Ci(_, CiChange::Lost { group, muid })] if *group == GROUP_0 && *muid == peer()
        ),
        "{ended:?}"
    );
    let sent = ci_sent(&wire);
    let [(group, CiMessage::InvalidateMuid { header, target })] = sent.as_slice() else {
        panic!("one Invalidate MUID, got {sent:?}");
    };
    assert_eq!((*group, header.source, *target), (GROUP_0, ours, ours));
    assert!(wire.session.ci_peers(&wire.id).is_empty());

    // Nothing more: a Discovery is not answered and the copy is off.
    let slot = wire.session.ports().list_input_ports()[0].id;
    let _ = wire.session.ports().drain_stream(slot).count();
    say(
        &wire,
        GROUP_0,
        &CiMessage::Discovery {
            header: CiHeader::new(peer()),
            data: peer_data(),
        },
        70,
    );
    assert_eq!(wire.session.ports().drain_stream(slot).count(), 0);
    let _ = polled(&wire, 80);
    assert!(received(&wire).is_empty());
}

/// A message is cut where the stream ring overflowed: the packets before the gap
/// and the ones after it are not one message. The payload is a valid Reply to
/// Discovery with padding after it (bytes past the last field are ignored), cut
/// in the padding, so joined it would be read and its sender found.
#[test]
fn a_gap_in_the_stream_ring_never_joins_a_midi_ci_message() {
    let (wire, ours) = midi1_running();
    let reply = |source: Muid| {
        let mut payload = CiMessage::DiscoveryReply {
            header: CiHeader::new(source),
            to: ours,
            data: peer_data(),
        }
        .encode();
        payload.extend([0u8; 24]);
        tutti_ump::Sysex7Packets::checked(GROUP_0, &payload)
            .unwrap()
            .collect::<Vec<_>>()
    };
    let cut = reply(peer());
    assert!(cut.len() >= 9, "{}", cut.len());
    let filler = Ump::from(Message::Stream(Stream::EndpointDiscovery {
        version: tutti_midi_device::UMP_VERSION,
        filter: tutti_ump::DiscoveryFilter {
            endpoint_info: true,
            device_identity: false,
            endpoint_name: false,
            product_instance_id: false,
            stream_configuration: false,
        },
    }));
    // The first six packets (the whole Reply, 36 bytes), then the ring is
    // filled to overflowing, so the next two are lost ...
    let (head, rest) = cut.split_at(6);
    reply_at(&wire, head.to_vec(), 40);
    for _ in 0..crate::port::async_port::STREAM_CAPACITY + 8 {
        reply_at(&wire, vec![filler], 41);
    }
    reply_at(&wire, rest[..2].to_vec(), 42);
    let _ = polled(&wire, 50);
    // ... and the End arrives after the gap.
    reply_at(&wire, rest[2..].to_vec(), 60);
    let spliced = polled(&wire, 70);
    assert!(
        spliced.is_empty(),
        "a message with a hole is no message: {spliced:?}"
    );
    assert!(wire.session.ci_peers(&wire.id).is_empty());

    // A whole message after it is read.
    let other = Muid::new(0x0065_4321).unwrap();
    reply_at(&wire, reply(other), 80);
    let found = polled(&wire, 90);
    assert!(
        matches!(found.as_slice(), [Polled::Ci(_, CiChange::Found(info))] if info.muid() == other),
        "{found:?}"
    );
}
