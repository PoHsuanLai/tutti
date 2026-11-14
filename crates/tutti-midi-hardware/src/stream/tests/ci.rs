//! MIDI-CI on the wire against a fake device, through
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
    Muid::of(0x0012_3456)
}

fn peer_data() -> DiscoveryData {
    peer_data_of(CiCategories::empty())
}

fn peer_data_of(categories: CiCategories) -> DiscoveryData {
    DiscoveryData::new(identity(), categories, MaxSysexSize::of(256))
}

fn polled(wire: &mut Wire, ms: u64) -> Vec<Polled> {
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
    let _ = polled(&mut wire, 100);
    wire
}

/// A MIDI 1.0 port whose Identity Request was answered and whose MIDI-CI has
/// started, with the MUID it drew; what was sent so far is read off.
fn midi1_running() -> (Wire, Muid) {
    let mut wire = midi1_wire();
    wire.session.enable_ci(identity());
    let _ = polled(&mut wire, 0);
    reply_at(&wire, identity_reply(0), 10);
    let _ = polled(&mut wire, 20);
    let _ = received(&wire);
    let _ = polled(&mut wire, 30);
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
    assert_eq!(payload.len(), 13 + 17);
    let Decoded::Message(CiMessage::Discovery { header, data }) = CiMessage::decode(payload) else {
        panic!("a Discovery");
    };
    assert_eq!(header.device_id, CiDeviceId::FunctionBlock);
    // The body (Table 6): exactly the app's identity (`identity()`: manufacturer
    // `41 00 00`, family 1 2, model 3 4, revision 5 6 7 8), Profile Configuration and Process Inquiry as the categories
    // (bits 2 and 4 of Table 7: the session initiates both), 512 as
    // four 7-bit bytes, Output Path 0.
    assert_eq!(payload[13..16], [0x41, 0x00, 0x00]);
    assert_eq!(payload[16..24], [1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!(data.identity, identity());
    assert_eq!(payload[24], 0b1_0100);
    assert_eq!(
        data.categories,
        CiCategories::PROFILE_CONFIGURATION | CiCategories::PROCESS_INQUIRY
    );
    assert_eq!(payload[25..29], [0x00, 0x04, 0x00, 0x00]);
    assert_eq!(payload[29], 0);
    assert_eq!(data.max_sysex_size, MaxSysexSize::of(512));
}

/// An Endpoint whose blocks all declare no MIDI-CI runs none: nothing is sent
/// however long it goes on, and the port does not copy its SysEx.
#[test]
fn an_endpoint_whose_blocks_declare_no_midi_ci_runs_none() {
    let mut wire = endpoint_wire(&[ci_block(0, 0, 0), ci_block(1, 3, 0)]);
    let slot = wire.session.ports().list_input_ports()[0].id;
    for ms in [200, 5_000, 20_000] {
        let _ = polled(&mut wire, ms);
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
    let mut wire = session_over(
        UmpCapability::midi2(),
        PortDirection::Duplex,
        Endpoint::new(Capabilities::default()),
    );
    wire.session.enable_ci(identity());
    wire.session.connect_input(&wire.id.clone()).unwrap();
    let step = STEP_TIMEOUT.as_millis() as u64;
    for ms in [0, step, 2 * step, 3 * step + 1, 3 * step + 2] {
        let _ = polled(&mut wire, ms);
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
    let mut wire = endpoint_wire(&[ci_block(0, 2, 2)]);
    let _ = received(&wire);
    let discovery = CiMessage::Discovery {
        header: CiHeader::new(peer()),
        data: peer_data(),
    };
    say(&wire, MidiGroup::of(5), &discovery, 200);
    let _ = polled(&mut wire, 210);
    assert!(ci_sent(&wire).is_empty(), "no device on Group 5");
    say(&wire, MidiGroup::of(2), &discovery, 220);
    let _ = polled(&mut wire, 230);
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
        let mut wire = midi1_wire();
        wire.session.enable_ci(identity());
        let mut order = String::new();
        for step in 0..8 {
            if answered && step == 1 {
                reply_at(&wire, identity_reply(0), step * ms - 1);
            }
            let _ = polled(&mut wire, step * ms);
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
    let (mut wire, ours) = midi1_running();
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
    let found = polled(&mut wire, 50);
    let [Polled::Ci(id, CiChange::Found(info))] = found.as_slice() else {
        panic!("one peer found, got {found:?}");
    };
    assert_eq!(*id, wire.id);
    assert_eq!((info.group(), info.muid()), (GROUP_0, peer()));
    assert_eq!(info.peer().discovery(), &peer_data());
    assert_eq!(
        &wire.session.ci_peers(&wire.id)[..],
        std::slice::from_ref(info)
    );
    assert!(polled(&mut wire, 60).is_empty(), "reported once");
}

/// Appendix E: a Responder answers Discovery from another device at any time,
/// long after its own window closed, with a Reply to that device that echoes
/// the Output Path ID (§5.6.1) and declares no Function Block (§5.6.2). A
/// Discovery is not a peer found.
#[test]
fn a_discovery_a_peer_sends_is_answered_long_after_the_window_closed() {
    let (mut wire, ours) = midi1_running();
    let _ = polled(&mut wire, 10_000);
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
    let none = polled(&mut wire, 10_002);
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
    let (mut wire, ours) = midi1_running();
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
    let _ = polled(&mut wire, 50);
    assert_eq!(wire.session.ci_peers(&wire.id).len(), 1);
    say(
        &wire,
        GROUP_0,
        &CiMessage::InvalidateMuid {
            header: CiHeader::new(Muid::of(0x00AB_CDEF)),
            target: peer(),
        },
        60,
    );
    let lost = polled(&mut wire, 70);
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
    let (mut wire, _) = midi1_running();
    reply_at(&wire, sysex(&[0x7E, 0x7F, 0x09, 0x01]), 40);
    assert!(polled(&mut wire, 50).is_empty());
    assert!(received(&wire).is_empty());
    assert!(wire.session.ci_peers(&wire.id).is_empty());
}

/// What MIDI-CI sends goes out on the input's own output and never the
/// session's MIDI output, and the SysEx it hears still reaches the graph: the
/// port copies it, it does not divert it.
#[test]
fn nothing_midi_ci_reaches_the_sessions_output_and_the_graph_still_hears_sysex() {
    use crate::testing::Rig;
    let mut wire = midi1_wire();
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

    let _ = polled(&mut wire, 0);
    reply_at(&wire, identity_reply(0), 1);
    let _ = polled(&mut wire, 2);
    let _ = polled(&mut wire, 3);
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
    let found = polled(&mut wire, 6);
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
    let (mut wire, ours) = midi1_running();
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
    let _ = polled(&mut wire, 50);
    assert_eq!(wire.session.ci_peers(&wire.id).len(), 1, "alive");

    assert!(wire.session.disconnect_input(&wire.id.clone()));
    // The close announced the MUID invalid, once; nothing follows it.
    assert_eq!(ci_sent(&wire).len(), 1);
    for ms in [60, 5_000, 60_000] {
        let _ = polled(&mut wire, ms);
    }
    assert!(received(&wire).is_empty());
    assert!(wire.session.ci_peers(&wire.id).is_empty());
}

/// A Destination the Reply to Discovery is for is one device, never Broadcast
/// (§3.3.3), as sent.
#[test]
fn a_reply_to_discovery_is_addressed_to_one_device() {
    let (mut wire, _) = midi1_running();
    say(
        &wire,
        GROUP_0,
        &CiMessage::Discovery {
            header: CiHeader::new(peer()),
            data: peer_data(),
        },
        40,
    );
    let _ = polled(&mut wire, 50);
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
    let mut wire = midi1_wire();
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
        let _ = polled(&mut wire, ms);
    }
    assert!(
        received(&wire).is_empty(),
        "a block that declares MIDI-CI, unasked"
    );
    assert_eq!(wire.session.ci_identity(), None);

    use tutti_midi_device::{INQUIRY_ATTEMPTS, INQUIRY_TIMEOUT};
    let mut wire = midi1_wire();
    let ms = INQUIRY_TIMEOUT.as_millis() as u64;
    let mut requests = 0;
    for step in 0..=u64::from(INQUIRY_ATTEMPTS) + 2 {
        let _ = polled(&mut wire, step * ms);
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
    let (mut wire, ours) = midi1_running();
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
    let _ = polled(&mut wire, 50);
    assert_eq!(wire.session.ci_peers(&wire.id).len(), 1);

    wire.session.disable_ci();
    let ended = polled(&mut wire, 60);
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
    let _ = polled(&mut wire, 80);
    assert!(received(&wire).is_empty());
}

/// `enable_ci` again: with the identity already in use it changes nothing; with
/// another, the running devices end as on a disable (an Invalidate MUID for
/// their own MUID, M2-101 §5.9, and their peers lost) and start again under a
/// new MUID, whose Discovery carries the new identity.
#[test]
fn enabling_again_with_another_identity_restarts_midi_ci_under_a_new_muid() {
    let (mut wire, ours) = midi1_running();
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
    let _ = polled(&mut wire, 50);
    assert_eq!(wire.session.ci_peers(&wire.id).len(), 1);

    wire.session.enable_ci(identity());
    assert!(
        polled(&mut wire, 60).is_empty(),
        "the same identity changes nothing"
    );
    assert!(ci_sent(&wire).is_empty());
    assert_eq!(wire.session.ci_peers(&wire.id).len(), 1);

    let mut other = identity();
    other.family = [U7::of(9), U7::of(9)];
    wire.session.enable_ci(other);
    let mut changes = polled(&mut wire, 70);
    changes.extend(polled(&mut wire, 80));
    assert!(
        changes.iter().any(|c| matches!(
            c,
            Polled::Ci(_, CiChange::Lost { muid, .. }) if *muid == peer()
        )),
        "{changes:?}"
    );
    let sent = ci_sent(&wire);
    let [
        (_, CiMessage::InvalidateMuid { header, target }),
        (_, CiMessage::Discovery { header: dh, data }),
    ] = sent.as_slice()
    else {
        panic!("an Invalidate MUID then a Discovery, got {sent:?}");
    };
    assert_eq!((header.source, *target), (ours, ours));
    assert_ne!(dh.source, ours, "a new MUID");
    assert_eq!(data.identity, other);
    assert!(wire.session.ci_peers(&wire.id).is_empty());
}

/// The peers read are a snapshot: a list held across a change keeps what it
/// read, a list read after it shows the change, and two reads with no change
/// between them share one list (a read copies nothing).
#[test]
fn the_peers_read_are_a_snapshot_shared_until_they_change() {
    let (mut wire, ours) = midi1_running();
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
    let _ = polled(&mut wire, 50);
    let held = wire.session.ci_peers(&wire.id);
    assert_eq!(held.len(), 1);
    assert!(std::sync::Arc::ptr_eq(
        &held,
        &wire.session.ci_peers(&wire.id)
    ));

    say(
        &wire,
        GROUP_0,
        &CiMessage::InvalidateMuid {
            header: CiHeader::new(peer()),
            target: peer(),
        },
        60,
    );
    let _ = polled(&mut wire, 70);
    assert_eq!(held.len(), 1, "a held list keeps what it read");
    assert_eq!(held[0].muid(), peer());
    assert!(wire.session.ci_peers(&wire.id).is_empty());
}

/// A message is cut where the stream ring overflowed: the packets before the gap
/// and the ones after it are not one message. The payload is a valid Reply to
/// Discovery with padding after it (bytes past the last field are ignored), cut
/// in the padding, so joined it would be read and its sender found.
#[test]
fn a_gap_in_the_stream_ring_never_joins_a_midi_ci_message() {
    let (mut wire, ours) = midi1_running();
    let reply = |source: Muid| {
        let mut payload = CiMessage::DiscoveryReply {
            header: CiHeader::new(source),
            to: ours,
            data: peer_data(),
        }
        .encode();
        payload.extend([0u8; 24]);
        tutti_ump::Sysex7Packets::from_payload(GROUP_0, &payload)
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
    let _ = polled(&mut wire, 50);
    // ... and the End arrives after the gap.
    reply_at(&wire, rest[2..].to_vec(), 60);
    let spliced = polled(&mut wire, 70);
    assert!(
        spliced.is_empty(),
        "a message with a hole is no message: {spliced:?}"
    );
    assert!(wire.session.ci_peers(&wire.id).is_empty());

    // A whole message after it is read.
    let other = Muid::of(0x0065_4321);
    reply_at(&wire, reply(other), 80);
    let found = polled(&mut wire, 90);
    assert!(
        matches!(found.as_slice(), [Polled::Ci(_, CiChange::Found(info))] if info.muid() == other),
        "{found:?}"
    );
}

/// The device tells the session its block `id` is now on `first_group`, one
/// Group wide, declaring MIDI-CI `version`.
fn block_info(wire: &Wire, id: u8, first_group: u8, version: u8, ms: u64) {
    send_ump(
        wire,
        Stream::FunctionBlockInfo(tutti_ump::FunctionBlockInfo {
            active: true,
            id: FunctionBlockId::new(id).unwrap(),
            direction: BlockDirection::Bidirectional,
            ui_hint: tutti_ump::UiHint::Both,
            midi1: tutti_ump::Midi1Port::No,
            first_group: FirstGroup::new(first_group).unwrap(),
            groups_spanned: GroupSpan::new(1).unwrap(),
            midi_ci_version: version,
            max_sysex8_streams: 0,
        }),
        ms,
    );
}

/// The MUID and Group of each Discovery among what was sent.
fn discoveries(wire: &Wire) -> Vec<(MidiGroup, Muid)> {
    discoveries_of(ci_sent(wire))
}

fn discoveries_of(sent: Vec<(MidiGroup, CiMessage)>) -> Vec<(MidiGroup, Muid)> {
    sent.into_iter()
        .filter_map(|(group, m)| match m {
            CiMessage::Discovery { header, .. } => Some((group, header.source)),
            _ => None,
        })
        .collect()
}

/// A MIDI 1.0 port whose peer declared `categories` and whose first inquiry
/// went out; the wire's clock is at 50 ms. What was sent is read off.
fn peer_declaring(categories: CiCategories) -> (Wire, Muid) {
    let (mut wire, ours) = midi1_running();
    say(
        &wire,
        GROUP_0,
        &CiMessage::DiscoveryReply {
            header: CiHeader::new(peer()),
            to: ours,
            data: peer_data_of(categories),
        },
        40,
    );
    let found = polled(&mut wire, 50);
    assert!(
        matches!(found.as_slice(), [Polled::Ci(_, CiChange::Found(_))]),
        "{found:?}"
    );
    let _ = received(&wire);
    (wire, ours)
}

/// A peer found on `group` by the device whose MUID is `ours`.
fn find_peer(wire: &mut Wire, group: MidiGroup, ours: Muid, ms: u64) {
    say(
        wire,
        group,
        &CiMessage::DiscoveryReply {
            header: CiHeader::new(peer()),
            to: ours,
            data: peer_data(),
        },
        ms,
    );
    let found = polled(wire, ms + 1);
    assert!(
        matches!(found.as_slice(), [Polled::Ci(_, CiChange::Found(_))]),
        "{found:?}"
    );
}

/// M2-101 §3.3.2: a block that starts declaring MIDI-CI after the settle is a
/// MIDI-CI Device from then on: its Group gets a Discovery, and the Group that
/// already had a device keeps it (nothing is sent for it).
#[test]
fn a_block_that_starts_declaring_midi_ci_after_the_settle_is_sent_discovery() {
    let mut wire = endpoint_wire(&[ci_block(0, 2, 2), ci_block(1, 4, 0)]);
    let _ = received(&wire);
    block_info(&wire, 1, 4, 2, 200);
    let _ = polled(&mut wire, 210);
    let _ = polled(&mut wire, 220);
    let sent = discoveries(&wire);
    assert!(
        matches!(sent.as_slice(), [(g, _)] if *g == MidiGroup::of(4)),
        "one Discovery, on the new Group: {sent:?}"
    );
}

/// §3.3.2 and §5.9: a block that stops declaring MIDI-CI ends its Group's
/// device (an Invalidate MUID from and for its own MUID, its peers `Lost`),
/// while another Group's device and peers are untouched.
#[test]
fn a_block_that_stops_declaring_midi_ci_ends_its_groups_device_and_no_other() {
    let mut wire = endpoint_wire(&[ci_block(0, 2, 2), ci_block(1, 4, 2)]);
    let muids: std::collections::HashMap<_, _> = discoveries(&wire).into_iter().collect();
    assert_eq!(muids.len(), 2);
    let (on2, on4) = (muids[&MidiGroup::of(2)], muids[&MidiGroup::of(4)]);
    find_peer(&mut wire, MidiGroup::of(2), on2, 200);
    find_peer(&mut wire, MidiGroup::of(4), on4, 220);
    let _ = received(&wire);

    block_info(&wire, 0, 2, 0, 300);
    // The poll that hears the change recomputes the Groups; the next acts.
    let mut all = polled(&mut wire, 310);
    all.extend(polled(&mut wire, 320));
    let lost: Vec<_> = all
        .iter()
        .filter_map(|p| match p {
            Polled::Ci(_, CiChange::Lost { group, muid }) => Some((*group, *muid)),
            _ => None,
        })
        .collect();
    assert_eq!(lost, [(MidiGroup::of(2), peer())]);
    let sent = ci_sent(&wire);
    assert!(
        matches!(
            sent.as_slice(),
            [(g, CiMessage::InvalidateMuid { header, target })]
                if *g == MidiGroup::of(2) && header.source == on2 && *target == on2
        ),
        "{sent:?}"
    );
    let peers = wire.session.ci_peers(&wire.id);
    assert_eq!(
        peers
            .iter()
            .map(|p| (p.group(), p.muid()))
            .collect::<Vec<_>>(),
        [(MidiGroup::of(4), peer())],
        "Group 4's peer is untouched"
    );
}

/// A block that moves to another Group is, by Group, an end and a start: the
/// old Group's device is invalidated and the new Group gets a Discovery under a
/// fresh MUID.
#[test]
fn a_block_that_moves_ends_the_old_groups_device_and_starts_the_new() {
    let mut wire = endpoint_wire(&[ci_block(0, 2, 2)]);
    let old = discoveries(&wire)[0].1;
    let _ = received(&wire);
    block_info(&wire, 0, 6, 2, 200);
    let _ = polled(&mut wire, 210);
    let _ = polled(&mut wire, 220);
    let sent = ci_sent(&wire);
    assert!(
        sent.iter().any(|(g, m)| *g == MidiGroup::of(2)
            && matches!(m, CiMessage::InvalidateMuid { target, .. } if *target == old)),
        "{sent:?}"
    );
    let started = discoveries_of(sent);
    assert!(
        matches!(started.as_slice(), [(g, muid)] if *g == MidiGroup::of(6) && *muid != old),
        "{started:?}"
    );
}

/// §5.9 lists a shutdown among the reasons for an Invalidate MUID: closing an
/// input sends one for each running device's own MUID, on that device's
/// output, and reports no peer lost (the app closed it).
#[test]
fn disconnecting_an_input_sends_one_invalidate_muid_from_our_muid() {
    let (mut wire, ours) = midi1_running();
    find_peer(&mut wire, GROUP_0, ours, 40);
    let _ = received(&wire);
    assert!(wire.session.disconnect_input(&wire.id.clone()));
    let sent = ci_sent(&wire);
    assert!(
        matches!(
            sent.as_slice(),
            [(g, CiMessage::InvalidateMuid { header, target })]
                if *g == GROUP_0 && header.source == ours && *target == ours
        ),
        "{sent:?}"
    );
    assert!(
        polled(&mut wire, 100).is_empty(),
        "no Lost for a closed input"
    );
}

/// Every input is closed the same way, by `disconnect_all_inputs`.
#[test]
fn disconnecting_all_inputs_invalidates_too() {
    let (mut wire, ours) = midi1_running();
    wire.session.disconnect_all_inputs();
    let sent = ci_sent(&wire);
    assert!(
        matches!(sent.as_slice(), [(_, CiMessage::InvalidateMuid { target, .. })] if *target == ours),
        "{sent:?}"
    );
}

/// An input whose MIDI-CI never ran has no MUID to invalidate: closing it
/// sends nothing, enabled or not.
#[test]
fn closing_an_input_without_midi_ci_sends_nothing() {
    let mut wire = midi1_wire();
    let _ = polled(&mut wire, 0);
    let _ = received(&wire);
    assert!(wire.session.disconnect_input(&wire.id.clone()));
    assert!(ci_sent(&wire).is_empty());
}

/// The device's own word (here its Identity Reply) is its identity; a MIDI-CI
/// peer fills only what that left, so its Product Instance Id (M2-101
/// §5.8.3.1, learned by Endpoint Inquiry) is added and its Discovery identity,
/// which speaks for a Function Block, never replaces the device's.
#[test]
fn a_ci_peers_product_instance_id_fills_what_the_device_left() {
    use tutti_midi_device::{CiInstanceId, EndpointInfo, ProductInstanceId};

    let (mut wire, ours) = midi1_running();
    let mut data = peer_data();
    data.identity = model_identity(9);
    data.function_block = Some(tutti_ump::FunctionBlockId::new(0).unwrap());
    say(
        &wire,
        GROUP_0,
        &CiMessage::DiscoveryReply {
            header: CiHeader::new(peer()),
            to: ours,
            data,
        },
        40,
    );
    assert!(matches!(
        polled(&mut wire, 50).as_slice(),
        [Polled::Ci(_, CiChange::Found(_))]
    ));
    let learned = wire.session.learned(&wire.id);
    assert_eq!(learned.identity(), Some(&identity()), "the device's own");
    assert_eq!(learned.instance_id(), None, "not asked yet");

    let serial = CiInstanceId::new(ProductInstanceId::new("SN-9").unwrap()).unwrap();
    say(
        &wire,
        GROUP_0,
        &CiMessage::EndpointReply {
            header: CiHeader::new(peer()),
            to: ours,
            info: EndpointInfo::ProductInstanceId(serial),
        },
        60,
    );
    assert!(matches!(
        polled(&mut wire, 70).as_slice(),
        [Polled::Ci(_, CiChange::Updated(_))]
    ));
    let learned = wire.session.learned(&wire.id);
    assert_eq!(learned.identity(), Some(&identity()));
    assert_eq!(
        learned.instance_id().map(ProductInstanceId::as_str),
        Some("SN-9")
    );
}

// --- Profile Configuration (M2-101 section 7, M2-102 section 2) -----------

use crate::{ChannelCount, ListedProfile, ProfileError, ProfilesChange, Whole};
use tutti_midi_device::{
    Outcome, PROFILE_TIMEOUT, ProfileChange, ProfileChannels, ProfileId, ProfileLists,
    ProfileState, Request, StandardProfile,
};
use tutti_ump::MidiChannel;

fn profile(number: u8) -> ProfileId {
    ProfileId::Standard(StandardProfile::new(
        U7::of(0x21),
        U7::of(number),
        U7::of(1),
        U7::of(1),
    ))
}

fn channel(n: u8) -> CiDeviceId {
    CiDeviceId::Channel(MidiChannel::of(n))
}

/// The peer's Reply to Profile Inquiry at `address`.
fn reply_at_address(
    wire: &Wire,
    ours: Muid,
    address: CiDeviceId,
    enabled: Vec<ProfileId>,
    disabled: Vec<ProfileId>,
    ms: u64,
) {
    say(
        wire,
        GROUP_0,
        &CiMessage::ProfileReply {
            header: CiHeader {
                device_id: address,
                ..CiHeader::new(peer())
            },
            to: ours,
            profiles: ProfileLists::new(enabled, disabled).unwrap(),
        },
        ms,
    );
}

/// Channel 1 lists `enabled` and `disabled`, the Group lists `group`, and the
/// final Reply on the Function Block ends the inquiry; the wire's clock is at
/// 100 ms.
fn known_profiles(
    wire: &mut Wire,
    ours: Muid,
    enabled: Vec<ProfileId>,
    disabled: Vec<ProfileId>,
    group: Vec<ProfileId>,
) {
    reply_at_address(wire, ours, channel(0), enabled, disabled, 60);
    reply_at_address(wire, ours, CiDeviceId::Group, vec![], group, 70);
    reply_at_address(wire, ours, CiDeviceId::FunctionBlock, vec![], vec![], 80);
    let changes = polled(wire, 100);
    assert!(
        matches!(
            changes.as_slice(),
            [Polled::Profile(_, ProfilesChange::Known { .. })]
        ),
        "{changes:?}"
    );
}

fn profile_events(polled: &[Polled]) -> Vec<&ProfilesChange> {
    polled
        .iter()
        .filter_map(|p| match p {
            Polled::Profile(_, change) => Some(change),
            _ => None,
        })
        .collect()
}

/// M2-101 section 7.2: the Initiator asks a peer that declared Profile
/// Configuration for its Profiles, with a Profile Inquiry (Sub-ID#2 `0x20`) to
/// the Function Block (`0x7F`), from its own MUID to the peer's. A peer that
/// declared none is not asked.
#[test]
fn a_peer_that_declares_profile_configuration_is_sent_a_profile_inquiry() {
    let (mut wire, ours) = midi1_running();
    say(
        &wire,
        GROUP_0,
        &CiMessage::DiscoveryReply {
            header: CiHeader::new(peer()),
            to: ours,
            data: peer_data_of(CiCategories::PROFILE_CONFIGURATION),
        },
        40,
    );
    let _ = polled(&mut wire, 50);
    let sent = payloads(&received(&wire));
    let [(group, payload)] = sent.as_slice() else {
        panic!("one message, the Profile Inquiry: {sent:?}");
    };
    assert_eq!(*group, GROUP_0);
    assert_eq!(payload[..4], [0x7E, 0x7F, 0x0D, 0x20]);
    assert_eq!(payload[5..9], ours.to_bytes(), "from our MUID");
    assert_eq!(payload[9..13], peer().to_bytes(), "to the peer's");

    // Another peer, which declares no category, is sent nothing.
    let other = Muid::of(0x0023_4567);
    say(
        &wire,
        GROUP_0,
        &CiMessage::DiscoveryReply {
            header: CiHeader::new(other),
            to: ours,
            data: peer_data(),
        },
        60,
    );
    let _ = polled(&mut wire, 70);
    assert!(ci_sent(&wire).is_empty());
    assert_eq!(wire.session.profiles(&wire.id).len(), 1);
}

/// M2-102 section 2.4: the peer answers a Function Block Profile Inquiry with a
/// Reply per address, and a final one on `0x7F` with no Profiles; that final
/// Reply makes the table the peer's whole declaration. Each Profile is listed
/// at the address it replied on, enabled or disabled as its list says, and a
/// Channel's is a handle that asks for a Channel count where a Group's has none.
#[test]
fn the_replies_to_profile_inquiry_become_the_peers_profiles_once_the_final_one_arrives() {
    let (mut wire, ours) = peer_declaring(CiCategories::PROFILE_CONFIGURATION);
    reply_at_address(
        &wire,
        ours,
        channel(0),
        vec![profile(1)],
        vec![profile(2)],
        60,
    );
    let _ = polled(&mut wire, 65);
    assert!(
        !wire.session.profiles(&wire.id).iter().any(|p| p.is_known()),
        "not known before the final Reply"
    );
    reply_at_address(&wire, ours, CiDeviceId::Group, vec![], vec![profile(3)], 70);
    reply_at_address(&wire, ours, CiDeviceId::FunctionBlock, vec![], vec![], 80);
    let changes = polled(&mut wire, 100);
    assert_eq!(
        profile_events(&changes),
        [&ProfilesChange::Known {
            group: GROUP_0,
            muid: peer()
        }]
    );
    let peers = wire.session.profiles(&wire.id);
    let [held] = &peers[..] else {
        panic!("one peer: {peers:?}");
    };
    assert!(held.is_known());
    assert_eq!((held.group(), held.muid()), (GROUP_0, peer()));
    let listed: Vec<_> = held.listed().collect();
    let [
        ListedProfile::Channel(one),
        ListedProfile::Channel(two),
        ListedProfile::Whole(three),
    ] = listed.as_slice()
    else {
        panic!("two on Channel 1 and one on the Group: {listed:?}");
    };
    assert_eq!(one.channel(), MidiChannel::of(0));
    assert_eq!(
        (listed[0].profile(), listed[0].state()),
        (profile(1), ProfileState::Enabled)
    );
    assert_eq!(
        (listed[1].profile(), listed[1].state()),
        (profile(2), ProfileState::Disabled)
    );
    assert_eq!(two.channel(), MidiChannel::of(0));
    assert_eq!(three.whole(), Whole::Group);
    assert_eq!(listed[2].state(), ProfileState::Disabled);
}

/// M2-101 section 7.8 and Table 24: a Set Profile On to a Channel goes to that
/// Channel's Device ID with Number Channels Requested (Message Version 2, two
/// bytes, LSB first); the Profile Enabled Report of section 7.10 is its
/// acknowledgement, resolving the request with the state and Channel count the
/// peer reports and updating what the session holds.
#[test]
fn a_set_profile_on_names_the_channel_and_its_enabled_report_updates_the_table() {
    let (mut wire, ours) = peer_declaring(CiCategories::PROFILE_CONFIGURATION);
    known_profiles(&mut wire, ours, vec![], vec![profile(2)], vec![]);
    let listed: Vec<_> = wire.session.profiles(&wire.id)[0].listed().collect();
    let [ListedProfile::Channel(c)] = listed.as_slice() else {
        panic!("one Channel Profile: {listed:?}");
    };
    let _ = received(&wire);
    wire.session
        .set_profile(c.enable(ChannelCount::new(3).unwrap()))
        .unwrap();
    assert!(received(&wire).is_empty(), "sent at the next poll");
    let none = polled(&mut wire, 110);
    assert!(none.is_empty(), "{none:?}");
    let sent = payloads(&received(&wire));
    let [(group, payload)] = sent.as_slice() else {
        panic!("one Set Profile On: {sent:?}");
    };
    assert_eq!(*group, GROUP_0);
    // F0 7E <Channel 1> 0D 22 <version> <source> <destination> <Profile Id> <count LSB first>
    assert_eq!(payload[..4], [0x7E, 0x00, 0x0D, 0x22]);
    assert_eq!(payload[5..9], ours.to_bytes());
    assert_eq!(payload[9..13], peer().to_bytes());
    assert_eq!(payload[13..18], profile(2).to_bytes());
    assert_eq!(payload[18..], [3, 0]);

    say(
        &wire,
        GROUP_0,
        &CiMessage::ProfileEnabled {
            header: CiHeader {
                device_id: channel(0),
                ..CiHeader::new(peer())
            },
            profile: profile(2),
            channels: ProfileChannels::of(3),
        },
        120,
    );
    let changes = polled(&mut wire, 130);
    assert_eq!(
        profile_events(&changes),
        [
            &ProfilesChange::Changed {
                group: GROUP_0,
                muid: peer(),
                address: channel(0),
                profile: profile(2),
                change: ProfileChange::Enabled,
            },
            &ProfilesChange::Resolved {
                group: GROUP_0,
                muid: peer(),
                request: Request::SetOn {
                    address: channel(0),
                    profile: profile(2)
                },
                outcome: Outcome::Reported {
                    state: ProfileState::Enabled,
                    channels: ProfileChannels::of(3),
                },
            }
        ]
    );
    let peers = wire.session.profiles(&wire.id);
    let entry = peers[0].table().get(channel(0), profile(2)).unwrap();
    assert_eq!(entry.state(), ProfileState::Enabled);
    assert_eq!(entry.channels(), Some(ProfileChannels::of(3)));
}

/// M2-120 section 3.2 (Table 4) names the MPE Profile, and the peer's Enabled
/// Report (appendix A.4) gives the Number of Channels, the Manager and its
/// Members: that is the zone the session reads. A disabled Profile, one
/// enabled with a single Channel (a Manager with no Member), and one on the
/// Group are no zone.
#[test]
fn an_enabled_mpe_profile_is_a_zone_of_the_channels_its_report_gave() {
    use tutti_midi_protocol::{MpeZoneConfig, PitchBendSensitivity};
    let (mut wire, ours) = peer_declaring(CiCategories::PROFILE_CONFIGURATION);
    known_profiles(&mut wire, ours, vec![], vec![ProfileId::MPE], vec![]);
    let zones = |wire: &Wire| wire.session.profiles(&wire.id)[0].mpe_zones();
    assert_eq!(zones(&wire), None, "supported, not enabled");
    for (channels, zone) in [
        (
            5,
            MpeZoneConfig::profile(
                MidiChannel::of(0),
                5,
                PitchBendSensitivity::MPE_PROFILE_DEFAULT,
            ),
        ),
        (1, None),
        (
            16,
            MpeZoneConfig::profile(
                MidiChannel::of(0),
                16,
                PitchBendSensitivity::MPE_PROFILE_DEFAULT,
            ),
        ),
    ] {
        say(
            &wire,
            GROUP_0,
            &CiMessage::ProfileEnabled {
                header: CiHeader {
                    device_id: channel(0),
                    ..CiHeader::new(peer())
                },
                profile: ProfileId::MPE,
                channels: ProfileChannels::of(channels),
            },
            120,
        );
        let _ = polled(&mut wire, 130);
        assert_eq!(
            zones(&wire),
            zone.map(tutti_midi_protocol::MpeZones::single),
            "{channels} channels"
        );
    }
    say(
        &wire,
        GROUP_0,
        &CiMessage::ProfileDisabled {
            header: CiHeader {
                device_id: channel(0),
                ..CiHeader::new(peer())
            },
            profile: ProfileId::MPE,
            channels: ProfileChannels::of(16),
        },
        140,
    );
    let _ = polled(&mut wire, 150);
    assert_eq!(zones(&wire), None, "disabled again");
}

/// M2-120 section 3.6.3: the Channel Response Type Notification is Profile
/// Specific Data of the MPE Profile on the zone's Manager Channel, one byte.
/// It reaches the app as a change, read as a [`ChannelResponse`]; the same
/// data of another Profile, or one that is not a notification, is not one.
#[test]
fn a_channel_response_notification_is_read_off_the_specific_data() {
    use tutti_midi_device::{Destination, SpecificData};
    use tutti_midi_protocol::ChannelResponse;
    let (mut wire, ours) = peer_declaring(CiCategories::PROFILE_CONFIGURATION);
    known_profiles(&mut wire, ours, vec![ProfileId::MPE], vec![], vec![]);
    let data = |profile: ProfileId, bytes: &[u8]| CiMessage::ProfileSpecificData {
        header: CiHeader {
            device_id: channel(3),
            ..CiHeader::new(peer())
        },
        to: Destination::Muid(ours),
        profile,
        data: SpecificData::new(bytes.to_vec()).unwrap(),
    };
    let mut read = Vec::new();
    for (ms, message) in [
        (120, data(ProfileId::MPE, &[0x01])),
        (130, data(ProfileId::MPE, &[0x00])),
        (140, data(ProfileId::MPE, &[0x02])),
        (150, data(profile(2), &[0x01])),
    ] {
        say(&wire, GROUP_0, &message, ms);
        let changes = polled(&mut wire, ms + 5);
        let events = profile_events(&changes);
        let [change] = events.as_slice() else {
            panic!("one change: {events:?}");
        };
        assert!(matches!(change, ProfilesChange::SpecificData { .. }));
        read.push(change.mpe_channel_response());
    }
    assert_eq!(
        read,
        [
            Some((MidiChannel::of(3), ChannelResponse::Monophonic)),
            Some((MidiChannel::of(3), ChannelResponse::Polyphonic)),
            None,
            None
        ]
    );
}

/// M2-101 section 7.8's note and 7.9: on the Group or the Function Block the
/// Number Channels Requested is 0x0000 (the width of what is addressed), and a
/// Set Profile Off (Sub-ID#2 `0x23`, Table 25) names only the Profile; the Disabled
/// Report of section 7.11 resolves it.
#[test]
fn a_set_profile_on_a_group_asks_for_no_channels_and_off_is_resolved_by_the_disabled_report() {
    let (mut wire, ours) = peer_declaring(CiCategories::PROFILE_CONFIGURATION);
    known_profiles(&mut wire, ours, vec![], vec![], vec![profile(3)]);
    let listed: Vec<_> = wire.session.profiles(&wire.id)[0].listed().collect();
    let [ListedProfile::Whole(whole)] = listed.as_slice() else {
        panic!("one Group Profile: {listed:?}");
    };
    let _ = received(&wire);
    wire.session.set_profile(whole.enable()).unwrap();
    let _ = polled(&mut wire, 110);
    let sent = payloads(&received(&wire));
    let [(_, on)] = sent.as_slice() else {
        panic!("{sent:?}");
    };
    assert_eq!(on[..4], [0x7E, 0x7E, 0x0D, 0x22], "to the Group");
    assert_eq!(on[18..], [0, 0], "no Channels requested");

    say(
        &wire,
        GROUP_0,
        &CiMessage::ProfileEnabled {
            header: CiHeader {
                device_id: CiDeviceId::Group,
                ..CiHeader::new(peer())
            },
            profile: profile(3),
            channels: ProfileChannels::of(0),
        },
        120,
    );
    let _ = polled(&mut wire, 130);
    let listed: Vec<_> = wire.session.profiles(&wire.id)[0].listed().collect();
    wire.session.set_profile(listed[0].disable()).unwrap();
    let _ = polled(&mut wire, 140);
    let sent = payloads(&received(&wire));
    let [(_, off)] = sent.as_slice() else {
        panic!("{sent:?}");
    };
    assert_eq!(off[..4], [0x7E, 0x7E, 0x0D, 0x23]);
    assert_eq!(off[13..18], profile(3).to_bytes());
    assert_eq!(off[18..], [0, 0], "the two reserved bytes of Version 2");

    say(
        &wire,
        GROUP_0,
        &CiMessage::ProfileDisabled {
            header: CiHeader {
                device_id: CiDeviceId::Group,
                ..CiHeader::new(peer())
            },
            profile: profile(3),
            channels: ProfileChannels::of(0),
        },
        150,
    );
    let changes = polled(&mut wire, 160);
    assert!(
        profile_events(&changes).iter().any(|c| matches!(
            c,
            ProfilesChange::Resolved {
                request: Request::SetOff { .. },
                outcome: Outcome::Reported {
                    state: ProfileState::Disabled,
                    ..
                },
                ..
            }
        )),
        "{changes:?}"
    );
    assert_eq!(
        wire.session.profiles(&wire.id)[0]
            .table()
            .get(CiDeviceId::Group, profile(3))
            .unwrap()
            .state(),
        ProfileState::Disabled
    );
}

/// M2-101 sections 7.4 and 7.5: a Profile Added Report lists a Profile (disabled)
/// and a Removed Report takes it out, each reported. A handle made before the
/// removal is then refused, since the peer no longer lists what it names.
#[test]
fn added_and_removed_reports_change_the_list_and_a_stale_handle_is_refused() {
    let (mut wire, ours) = peer_declaring(CiCategories::PROFILE_CONFIGURATION);
    known_profiles(&mut wire, ours, vec![], vec![profile(2)], vec![]);
    let stale: Vec<_> = wire.session.profiles(&wire.id)[0].listed().collect();
    say(
        &wire,
        GROUP_0,
        &CiMessage::ProfileAdded {
            header: CiHeader {
                device_id: channel(0),
                ..CiHeader::new(peer())
            },
            profile: profile(4),
        },
        110,
    );
    let changes = polled(&mut wire, 120);
    assert!(
        matches!(
            profile_events(&changes).as_slice(),
            [ProfilesChange::Changed { profile: p, change: ProfileChange::Added, .. }] if *p == profile(4)
        ),
        "{changes:?}"
    );
    assert_eq!(wire.session.profiles(&wire.id)[0].listed().count(), 2);

    say(
        &wire,
        GROUP_0,
        &CiMessage::ProfileRemoved {
            header: CiHeader {
                device_id: channel(0),
                ..CiHeader::new(peer())
            },
            profile: profile(2),
        },
        130,
    );
    let changes = polled(&mut wire, 140);
    assert!(
        matches!(
            profile_events(&changes).as_slice(),
            [ProfilesChange::Changed { profile: p, change: ProfileChange::Removed, .. }] if *p == profile(2)
        ),
        "{changes:?}"
    );
    let ListedProfile::Channel(old) = &stale[0] else {
        panic!("a Channel Profile");
    };
    assert_eq!(
        wire.session.set_profile(old.enable(ChannelCount::ONE)),
        Err(ProfileError::NotListed)
    );
}

/// M2-101 section 5.9: an Invalidate MUID for the peer discards it, and with it
/// its Initiator and the Profiles it held: they are reported dropped, the
/// session holds none, and a handle made before is refused, as is a switch
/// queued before it that the next poll would have sent.
#[test]
fn an_invalidate_muid_drops_the_peers_profiles_and_refuses_its_handles() {
    let (mut wire, ours) = peer_declaring(CiCategories::PROFILE_CONFIGURATION);
    known_profiles(&mut wire, ours, vec![], vec![profile(2)], vec![]);
    let listed: Vec<_> = wire.session.profiles(&wire.id)[0].listed().collect();
    let ListedProfile::Channel(c) = &listed[0] else {
        panic!("a Channel Profile");
    };
    wire.session
        .set_profile(c.enable(ChannelCount::ONE))
        .unwrap();
    say(
        &wire,
        GROUP_0,
        &CiMessage::InvalidateMuid {
            header: CiHeader::new(Muid::of(0x00AB_CDEF)),
            target: peer(),
        },
        110,
    );
    let changes = polled(&mut wire, 120);
    assert!(
        profile_events(&changes).contains(&&ProfilesChange::Dropped {
            group: GROUP_0,
            muid: peer()
        }),
        "{changes:?}"
    );
    assert!(wire.session.profiles(&wire.id).is_empty());
    assert!(
        payloads(&received(&wire)).iter().all(|(_, p)| p[3] != 0x22),
        "the queued switch was not sent to a peer that is gone"
    );
    assert_eq!(
        wire.session.set_profile(c.enable(ChannelCount::ONE)),
        Err(ProfileError::NoSuchPeer)
    );
}

/// Disabling MIDI-CI ends every device and its peers; the peers' Profiles are
/// dropped and reported with them.
#[test]
fn disabling_midi_ci_drops_the_profiles() {
    let (mut wire, ours) = peer_declaring(CiCategories::PROFILE_CONFIGURATION);
    known_profiles(&mut wire, ours, vec![], vec![profile(2)], vec![]);
    wire.session.disable_ci();
    let changes = polled(&mut wire, 110);
    assert!(
        profile_events(&changes).contains(&&ProfilesChange::Dropped {
            group: GROUP_0,
            muid: peer()
        }),
        "{changes:?}"
    );
    assert!(wire.session.profiles(&wire.id).is_empty());
}

/// M2-102 gives an Initiator no time limit; tutti's is [`PROFILE_TIMEOUT`]. A
/// Set Profile On the peer never answers ends, and is reported, once that long
/// has passed since it was sent and not before; the table is left as it was.
#[test]
fn a_set_profile_nothing_answers_times_out_after_the_profile_timeout() {
    let (mut wire, ours) = peer_declaring(CiCategories::PROFILE_CONFIGURATION);
    known_profiles(&mut wire, ours, vec![], vec![profile(2)], vec![]);
    let listed: Vec<_> = wire.session.profiles(&wire.id)[0].listed().collect();
    let ListedProfile::Channel(c) = &listed[0] else {
        panic!("a Channel Profile");
    };
    wire.session
        .set_profile(c.enable(ChannelCount::ONE))
        .unwrap();
    let sent_at = 200;
    let _ = polled(&mut wire, sent_at);
    let wait = PROFILE_TIMEOUT.as_millis() as u64;
    let early = polled(&mut wire, sent_at + wait - 1);
    assert!(profile_events(&early).is_empty(), "{early:?}");
    let ended = polled(&mut wire, sent_at + wait);
    assert!(
        matches!(
            profile_events(&ended).as_slice(),
            [ProfilesChange::Resolved {
                outcome: Outcome::TimedOut,
                ..
            }]
        ),
        "{ended:?}"
    );
    assert_eq!(
        wire.session.profiles(&wire.id)[0]
            .table()
            .get(channel(0), profile(2))
            .unwrap()
            .state(),
        ProfileState::Disabled
    );
}

/// A peer that answers only part of the Profile Inquiry, never the final Reply,
/// is given up on after [`PROFILE_TIMEOUT`]: it is reported, the table keeps
/// what was heard, and it is not known to be whole.
#[test]
fn an_inquiry_without_a_final_reply_times_out_keeping_what_was_heard() {
    let (mut wire, ours) = peer_declaring(CiCategories::PROFILE_CONFIGURATION);
    reply_at_address(&wire, ours, channel(0), vec![profile(1)], vec![], 60);
    let _ = polled(&mut wire, 70);
    let wait = PROFILE_TIMEOUT.as_millis() as u64;
    let ended = polled(&mut wire, 50 + wait);
    assert_eq!(
        profile_events(&ended),
        [&ProfilesChange::InquiryTimedOut {
            group: GROUP_0,
            muid: peer()
        }]
    );
    let peers = wire.session.profiles(&wire.id);
    assert!(!peers[0].is_known());
    assert_eq!(peers[0].listed().count(), 1);
}

/// A second request for a Profile at an address whose first is still waiting is
/// not sent: it is reported as not sent, and only one Set Profile On went out.
#[test]
fn a_request_while_one_for_that_profile_waits_is_not_sent() {
    let (mut wire, ours) = peer_declaring(CiCategories::PROFILE_CONFIGURATION);
    known_profiles(&mut wire, ours, vec![], vec![profile(2)], vec![]);
    let listed: Vec<_> = wire.session.profiles(&wire.id)[0].listed().collect();
    let ListedProfile::Channel(c) = &listed[0] else {
        panic!("a Channel Profile");
    };
    let _ = received(&wire);
    for _ in 0..2 {
        wire.session
            .set_profile(c.enable(ChannelCount::ONE))
            .unwrap();
    }
    let changes = polled(&mut wire, 110);
    assert!(
        matches!(
            profile_events(&changes).as_slice(),
            [ProfilesChange::NotSent { profile: p, .. }] if *p == profile(2)
        ),
        "{changes:?}"
    );
    assert_eq!(payloads(&received(&wire)).len(), 1);
}

/// A count of Channels is at least 1 and fits 14 bits (M2-101 section 7.8).
#[test]
fn a_channel_count_is_at_least_one_and_fits_fourteen_bits() {
    assert_eq!(ChannelCount::new(0), None);
    assert_eq!(ChannelCount::new(1), Some(ChannelCount::ONE));
    assert_eq!(
        ChannelCount::new(16_383).map(ChannelCount::get),
        Some(16_383)
    );
    assert_eq!(ChannelCount::new(16_384), None);
}

use crate::{ProcessChange, ReportError};
use tutti_midi_device::{
    ChannelControllers, DataControl, Nak, NoteData, PROCESS_TIMEOUT, ProcessError, ProcessFeatures,
    ReportRequest, ReportScope, SUB_ID2_MESSAGE_REPORT_INQUIRY, SystemMessages,
};

fn process_events(polled: &[Polled]) -> Vec<&ProcessChange> {
    polled
        .iter()
        .filter_map(|p| match p {
            Polled::Process(_, change) => Some(change),
            _ => None,
        })
        .collect()
}

/// The peer answers the capabilities inquiry with `features`; the clock is at
/// 70 ms.
fn answer_capabilities(wire: &mut Wire, ours: Muid, features: ProcessFeatures) {
    say(
        wire,
        GROUP_0,
        &CiMessage::ProcessCapabilitiesReply {
            header: CiHeader {
                device_id: CiDeviceId::FunctionBlock,
                ..CiHeader::new(peer())
            },
            to: ours,
            features,
        },
        60,
    );
    let changes = polled(wire, 70);
    assert_eq!(
        process_events(&changes),
        [&ProcessChange::Capabilities {
            group: GROUP_0,
            muid: peer(),
            features
        }]
    );
}

fn function_block_request() -> ReportRequest {
    ReportRequest::new(
        DataControl::NonDefault,
        ReportScope::FunctionBlock {
            system: SystemMessages::SONG_POSITION,
            controllers: ChannelControllers::CONTROL_CHANGE,
            notes: NoteData::NOTES,
        },
    )
}

/// M2-101 section 9.2 and Table 7: the Initiator asks a peer that declared
/// Process Inquiry for its capabilities, with an Inquiry: Process Inquiry
/// Capabilities (Sub-ID#2 `0x40`) to the Function Block (`0x7F`), from its own
/// MUID to the peer's. A peer that declared none is not asked.
#[test]
fn a_peer_that_declares_process_inquiry_is_sent_a_capabilities_inquiry() {
    let (mut wire, ours) = midi1_running();
    say(
        &wire,
        GROUP_0,
        &CiMessage::DiscoveryReply {
            header: CiHeader::new(peer()),
            to: ours,
            data: peer_data_of(CiCategories::PROCESS_INQUIRY),
        },
        40,
    );
    let _ = polled(&mut wire, 50);
    let sent = payloads(&received(&wire));
    let [(group, payload)] = sent.as_slice() else {
        panic!("one message, the capabilities inquiry: {sent:?}");
    };
    assert_eq!(*group, GROUP_0);
    // F0 7E 7F 0D 40 <version> <source> <destination> F7
    assert_eq!(payload[..4], [0x7E, 0x7F, 0x0D, 0x40]);
    assert_eq!(payload[5..9], ours.to_bytes(), "from our MUID");
    assert_eq!(payload[9..13], peer().to_bytes(), "to the peer's");
    assert_eq!(payload.len(), 13);
    assert!(
        wire.session.report_peers(&wire.id).is_empty(),
        "not yet answered"
    );

    // A peer that declares no category is sent nothing.
    let other = Muid::of(0x0023_4567);
    say(
        &wire,
        GROUP_0,
        &CiMessage::DiscoveryReply {
            header: CiHeader::new(other),
            to: ours,
            data: peer_data(),
        },
        60,
    );
    let _ = polled(&mut wire, 70);
    assert!(ci_sent(&wire).is_empty());
}

/// Table 42: a reply without D0, MIDI Message Report, leaves the peer one a report
/// cannot be asked of: it has no handle.
#[test]
fn a_peer_whose_capabilities_lack_midi_message_report_has_no_handle() {
    let (mut wire, ours) = peer_declaring(CiCategories::PROCESS_INQUIRY);
    answer_capabilities(&mut wire, ours, ProcessFeatures::empty());
    assert!(wire.session.report_peers(&wire.id).is_empty());
}

/// M2-101 section 9.5: the Inquiry: MIDI Message Report (Sub-ID#2 `0x42`) carries
/// the Message Data Control and the bitmaps of Table 43, on the Device ID of its
/// scope. Its Reply (`0x43`) and End (`0x44`) are reported as they arrive, and
/// the transaction is over.
#[test]
fn a_report_asks_with_table_43_and_ends_with_the_peers_end() {
    let (mut wire, ours) = peer_declaring(CiCategories::PROCESS_INQUIRY);
    answer_capabilities(&mut wire, ours, ProcessFeatures::MIDI_MESSAGE_REPORT);
    let listed = wire.session.report_peers(&wire.id);
    let [handle] = &listed[..] else {
        panic!("one reporter: {listed:?}");
    };
    assert_eq!((handle.group(), handle.muid()), (GROUP_0, peer()));
    let _ = received(&wire);

    wire.session
        .report(handle.ask(function_block_request()))
        .unwrap();
    assert!(received(&wire).is_empty(), "sent at the next poll");
    let none = polled(&mut wire, 110);
    assert!(none.is_empty(), "{none:?}");
    let sent = payloads(&received(&wire));
    let [(group, payload)] = sent.as_slice() else {
        panic!("one Inquiry: {sent:?}");
    };
    assert_eq!(*group, GROUP_0);
    assert_eq!(payload[..4], [0x7E, 0x7F, 0x0D, 0x42]);
    assert_eq!(payload[5..9], ours.to_bytes());
    assert_eq!(payload[9..13], peer().to_bytes());
    // Control 0x01, system D1, the reserved byte, controllers D1, notes D0.
    assert_eq!(payload[13..], [0x01, 0b10, 0x00, 0b10, 0b1]);

    let declared = ReportScope::FunctionBlock {
        system: SystemMessages::empty(),
        controllers: ChannelControllers::CONTROL_CHANGE,
        notes: NoteData::NOTES,
    };
    say(
        &wire,
        GROUP_0,
        &CiMessage::MessageReportReply {
            header: CiHeader {
                device_id: CiDeviceId::FunctionBlock,
                ..CiHeader::new(peer())
            },
            to: ours,
            scope: declared,
        },
        120,
    );
    let began = polled(&mut wire, 130);
    assert_eq!(
        process_events(&began),
        [&ProcessChange::ReportBegan {
            group: GROUP_0,
            muid: peer(),
            declared
        }]
    );
    say(
        &wire,
        GROUP_0,
        &CiMessage::MessageReportEnd {
            header: CiHeader {
                device_id: CiDeviceId::FunctionBlock,
                ..CiHeader::new(peer())
            },
            to: ours,
        },
        140,
    );
    let ended = polled(&mut wire, 150);
    assert_eq!(
        process_events(&ended),
        [&ProcessChange::ReportEnded {
            group: GROUP_0,
            muid: peer()
        }]
    );
    // The transaction is over: another may be asked for.
    wire.session
        .report(handle.ask(function_block_request()))
        .unwrap();
    let again = polled(&mut wire, 160);
    assert!(again.is_empty(), "{again:?}");
    assert_eq!(payloads(&received(&wire)).len(), 1);
}

/// M2-101 section 9.8.1: a NAK ends a report even with no End.
#[test]
fn a_nak_of_a_report_ends_it_and_is_reported() {
    let (mut wire, ours) = peer_declaring(CiCategories::PROCESS_INQUIRY);
    answer_capabilities(&mut wire, ours, ProcessFeatures::MIDI_MESSAGE_REPORT);
    let handle = wire.session.report_peers(&wire.id)[0].clone();
    wire.session
        .report(handle.ask(function_block_request()))
        .unwrap();
    let _ = polled(&mut wire, 110);
    let nak = Nak::new(
        U7::of(SUB_ID2_MESSAGE_REPORT_INQUIRY),
        Nak::STATUS_TERMINATE,
    );
    say(
        &wire,
        GROUP_0,
        &CiMessage::Nak {
            header: CiHeader {
                device_id: CiDeviceId::FunctionBlock,
                ..CiHeader::new(peer())
            },
            to: ours,
            nak: nak.clone(),
        },
        120,
    );
    let ended = polled(&mut wire, 130);
    assert_eq!(
        process_events(&ended),
        [&ProcessChange::ReportRefused {
            group: GROUP_0,
            muid: peer(),
            nak
        }]
    );
}

/// A report whose Reply never comes ends once the process timeout has passed
/// since it was sent, and not before.
#[test]
fn a_report_nothing_answers_times_out_after_the_process_timeout() {
    let (mut wire, ours) = peer_declaring(CiCategories::PROCESS_INQUIRY);
    answer_capabilities(&mut wire, ours, ProcessFeatures::MIDI_MESSAGE_REPORT);
    let handle = wire.session.report_peers(&wire.id)[0].clone();
    wire.session
        .report(handle.ask(function_block_request()))
        .unwrap();
    let sent_at = 200;
    let _ = polled(&mut wire, sent_at);
    let wait = PROCESS_TIMEOUT.as_millis() as u64;
    let early = polled(&mut wire, sent_at + wait - 1);
    assert!(process_events(&early).is_empty(), "{early:?}");
    let ended = polled(&mut wire, sent_at + wait);
    assert_eq!(
        process_events(&ended),
        [&ProcessChange::ReportTimedOut {
            group: GROUP_0,
            muid: peer()
        }]
    );
}

/// A second request while one waits is not sent: it is reported, and only one
/// Inquiry went out.
#[test]
fn a_report_while_one_waits_is_not_sent() {
    let (mut wire, ours) = peer_declaring(CiCategories::PROCESS_INQUIRY);
    answer_capabilities(&mut wire, ours, ProcessFeatures::MIDI_MESSAGE_REPORT);
    let handle = wire.session.report_peers(&wire.id)[0].clone();
    let _ = received(&wire);
    for _ in 0..2 {
        wire.session
            .report(handle.ask(function_block_request()))
            .unwrap();
    }
    let changes = polled(&mut wire, 110);
    assert_eq!(
        process_events(&changes),
        [&ProcessChange::NotSent {
            group: GROUP_0,
            muid: peer(),
            error: ProcessError::InFlight
        }]
    );
    assert_eq!(payloads(&received(&wire)).len(), 1);
}

/// A lost peer's handle is refused, and the peer's Initiator is reported dropped.
#[test]
fn a_lost_peer_drops_its_process_inquiry_and_refuses_its_handles() {
    let (mut wire, ours) = peer_declaring(CiCategories::PROCESS_INQUIRY);
    answer_capabilities(&mut wire, ours, ProcessFeatures::MIDI_MESSAGE_REPORT);
    let handle = wire.session.report_peers(&wire.id)[0].clone();
    say(
        &wire,
        GROUP_0,
        &CiMessage::InvalidateMuid {
            header: CiHeader::new(peer()),
            target: peer(),
        },
        80,
    );
    let changes = polled(&mut wire, 90);
    assert!(
        process_events(&changes).contains(&&ProcessChange::Dropped {
            group: GROUP_0,
            muid: peer()
        }),
        "{changes:?}"
    );
    assert!(wire.session.report_peers(&wire.id).is_empty());
    assert_eq!(
        wire.session.report(handle.ask(function_block_request())),
        Err(ReportError::NoSuchPeer)
    );
}

/// Disabling MIDI-CI drops the peers' Process Inquiry.
#[test]
fn disabling_midi_ci_drops_process_inquiry() {
    let (mut wire, ours) = peer_declaring(CiCategories::PROCESS_INQUIRY);
    answer_capabilities(&mut wire, ours, ProcessFeatures::MIDI_MESSAGE_REPORT);
    wire.session.disable_ci();
    let changes = polled(&mut wire, 110);
    assert!(
        process_events(&changes).contains(&&ProcessChange::Dropped {
            group: GROUP_0,
            muid: peer()
        }),
        "{changes:?}"
    );
    assert!(wire.session.report_peers(&wire.id).is_empty());
}

/// M2-101 section 9.3 and 9.6: the session declares Process Inquiry, and answers a
/// capabilities inquiry with D0, and a report of the Function Block with an empty
/// Reply (`43`, all bitmaps 0) and the End (`44`) on the same Device ID.
#[test]
fn the_session_answers_a_process_inquiry_as_a_device_with_nothing_to_report() {
    let (mut wire, ours) = midi1_running();
    say(
        &wire,
        GROUP_0,
        &CiMessage::ProcessCapabilitiesInquiry {
            header: CiHeader {
                device_id: CiDeviceId::FunctionBlock,
                ..CiHeader::new(peer())
            },
            to: ours,
        },
        40,
    );
    say(
        &wire,
        GROUP_0,
        &CiMessage::MessageReportInquiry {
            header: CiHeader {
                device_id: CiDeviceId::Group,
                ..CiHeader::new(peer())
            },
            to: ours,
            control: DataControl::Full,
            scope: ReportScope::Group {
                controllers: ChannelControllers::all(),
                notes: NoteData::all(),
            },
        },
        45,
    );
    let _ = polled(&mut wire, 50);
    let sent = payloads(&received(&wire));
    let [(_, caps), (_, begin), (_, end)] = sent.as_slice() else {
        panic!("a reply, a Reply and an End: {sent:?}");
    };
    assert_eq!(caps[..4], [0x7E, 0x7F, 0x0D, 0x41]);
    assert_eq!(caps[13..], [0x01]);
    assert_eq!(begin[..4], [0x7E, 0x7E, 0x0D, 0x43]);
    assert_eq!(begin[9..13], peer().to_bytes());
    assert_eq!(begin[13..], [0, 0, 0, 0]);
    assert_eq!(end[..4], [0x7E, 0x7E, 0x0D, 0x44]);
    assert_eq!(end.len(), 13);
}
