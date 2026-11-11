use std::time::{Duration, Instant};

use tutti_ump::{
    DeviceIdentity, Direction as BlockDirection, FirstGroup, Form, FunctionBlockId,
    FunctionBlockInfo, GroupSpan, Message, Midi1Port, Protocol, Stream, StreamConfig, U7, UiHint,
    Ump, decode,
};

use super::*;
use crate::endpoint::{Capabilities, Endpoint, FunctionBlock};
use crate::name::{BlockName, EndpointName, ProductInstanceId};

fn at(base: Instant, ms: u64) -> Instant {
    base + Duration::from_millis(ms)
}

fn msg(u: Ump) -> Message {
    decode(u).unwrap()
}

fn stream(s: Stream) -> Message {
    Message::Stream(s)
}

const FULL: Capabilities = Capabilities {
    midi1: true,
    midi2: true,
    rx_jr: true,
    tx_jr: true,
};

fn id(n: u8) -> FunctionBlockId {
    FunctionBlockId::new(n).unwrap()
}

fn block(n: u8, first: u8, span: u8, name: &str) -> FunctionBlock {
    FunctionBlock::new(
        id(n),
        BlockDirection::Bidirectional,
        FirstGroup::new(first).unwrap(),
        GroupSpan::new(span).unwrap(),
    )
    .unwrap()
    .with_name(BlockName::new(name).unwrap())
}

/// A MIDI 2.0 device with every optional part and two blocks.
fn device() -> Endpoint {
    let u = |n| U7::new(n).unwrap();
    Endpoint::new(FULL)
        .with_identity(DeviceIdentity {
            manufacturer: [u(0), u(0x21), u(0x09)],
            family: [u(1), u(2)],
            model: [u(3), u(4)],
            revision: [u(5), u(6), u(7), u(8)],
        })
        .with_name(EndpointName::new("A long endpoint name of 40 characters!").unwrap())
        .with_instance_id(ProductInstanceId::new("SERIAL-0123456789-ABCDEFGHIJ").unwrap())
        .with_static_function_blocks(true)
        .with_function_block(block(0, 0, 1, "Keys"))
        .with_function_block(
            block(1, 1, 2, "A function block name past thirteen bytes").with_midi_ci(2, 2),
        )
}

/// What a connection sent, answered by `peer` until it stops sending.
fn run(
    connection: &mut Connection,
    peer: &mut Endpoint,
    first: Ump,
    now: Instant,
) -> (Action, Vec<Ump>) {
    let mut sent = vec![first];
    let mut packet = first;
    loop {
        let mut replies: Vec<Ump> = Vec::new();
        peer.reply(msg(packet), &mut replies);
        let mut next = None;
        for reply in replies {
            match connection.on_message(msg(reply), now) {
                Action::Send(p) => {
                    sent.push(p);
                    next = Some(p);
                }
                Action::Settled(e) => return (Action::Settled(e), sent),
                Action::Wait => {}
            }
        }
        match next {
            Some(p) => packet = p,
            None => return (Action::Wait, sent),
        }
    }
}

fn discovery_words() -> [u32; 4] {
    // M2-104 §7.1.1: version 1.1, every filter bit (e d n i s).
    [0xF000_0101, 0x0000_001F, 0, 0]
}

// M2-100 Table 4 steps 2 to 4 against a MIDI 2.0 device: Discovery, a Stream
// Configuration Request for MIDI 2.0 with JR both ways (§7.1.6.2: word 0
// `0x02 << 8 | RXJR << 1 | TXJR`), then a Function Block Discovery for all
// blocks, Info and Name (§7.1.7: block 0xFF, bits `n` and `i`).
#[test]
fn the_happy_path_reaches_midi2_with_jr() {
    let t0 = Instant::now();
    let (mut c, first) = Connection::new(t0);
    assert_eq!(first.0, discovery_words());
    assert_eq!(c.deadline(), Some(at(t0, 1000)));

    let mut peer = device();
    let (end, sent) = run(&mut c, &mut peer, first, at(t0, 10));
    assert_eq!(end, Action::Settled(Ending::Negotiated));
    assert_eq!(sent.len(), 3);
    assert_eq!(sent[1].0, [0xF005_0203, 0, 0, 0]);
    assert_eq!(sent[2].0, [0xF010_FF03, 0, 0, 0]);
    assert_eq!(c.settled(), Some(Ending::Negotiated));
    assert_eq!(c.deadline(), None);

    // The peer now holds the configuration it was asked for, and the
    // discoverer has learned everything the peer declared, names included.
    assert_eq!(
        c.endpoint().active(),
        StreamConfig {
            protocol: Protocol::Midi2,
            rx_jr: true,
            tx_jr: true
        }
    );
    assert_eq!(*c.endpoint(), peer);
    assert_eq!(
        c.endpoint().name().unwrap().as_str(),
        "A long endpoint name of 40 characters!"
    );
    assert_eq!(
        c.endpoint()
            .function_block(id(1))
            .unwrap()
            .name()
            .unwrap()
            .as_str(),
        "A function block name past thirteen bytes"
    );
    assert_eq!(
        c.endpoint()
            .function_block(id(1))
            .unwrap()
            .midi_ci_version(),
        2
    );
}

// A peer that supports only MIDI 1.0 gets no Stream Configuration Request: what
// it reports already is what is wanted. Its one block has no name, so the
// connection settles when the NAME_GRACE after its Info has passed.
#[test]
fn a_midi1_peer_is_not_asked_to_change() {
    let t0 = Instant::now();
    let (mut c, first) = Connection::new(t0);
    let mut peer = Endpoint::midi1_device(EndpointName::new("Keystep"));
    let (end, sent) = run(&mut c, &mut peer, first, t0);
    assert_eq!(end, Action::Wait);
    assert!(
        sent.iter().all(|p| (p.0[0] >> 16) & 0x3FF != 0x05),
        "no Stream Configuration Request"
    );
    assert_eq!(sent[1].0, [0xF010_FF03, 0, 0, 0]);
    assert_eq!(c.deadline(), Some(at(t0, 100)));
    assert_eq!(c.on_timeout(at(t0, 99)), Action::Wait);
    assert_eq!(
        c.on_timeout(at(t0, 100)),
        Action::Settled(Ending::Negotiated)
    );
    assert_eq!(c.endpoint().active().protocol, Protocol::Midi1);
    assert_eq!(c.endpoint().name().unwrap().as_str(), "Keystep");
    assert_eq!(c.endpoint().function_blocks().count(), 1);
}

// Each part the peer lacks is dropped from the request (§7.1.6.1 step 2 asks
// for the desired configuration; §7.1.2 says what is supported).
#[test]
fn the_request_asks_only_for_what_the_peer_supports() {
    let t0 = Instant::now();
    let (mut c, first) = Connection::new(t0);
    let mut peer = Endpoint::new(Capabilities {
        midi1: true,
        midi2: true,
        rx_jr: true,
        tx_jr: false,
    });
    let (_, sent) = run(&mut c, &mut peer, first, t0);
    assert_eq!(sent[1].0, [0xF005_0202, 0, 0, 0]);
    assert_eq!(c.endpoint().active(), peer.active());
    // And the other direction.
    let (mut c, first) = Connection::new(t0);
    let mut peer = Endpoint::new(Capabilities {
        midi1: true,
        midi2: true,
        rx_jr: false,
        tx_jr: true,
    });
    let (_, sent) = run(&mut c, &mut peer, first, t0);
    assert_eq!(sent[1].0, [0xF005_0201, 0, 0, 0]);
    assert_eq!(c.endpoint().active(), peer.active());
}

#[test]
fn a_wanted_midi1_is_requested_of_a_midi2_peer() {
    let t0 = Instant::now();
    let wanted = StreamConfig {
        protocol: Protocol::Midi1,
        rx_jr: false,
        tx_jr: false,
    };
    let (mut c, first) = Connection::with_wanted(wanted, t0);
    let mut peer = device();
    // The device starts in MIDI 1.0 without JR, which is what is wanted.
    let (_, sent) = run(&mut c, &mut peer, first, t0);
    assert!(sent.iter().all(|p| (p.0[0] >> 16) & 0x3FF != 0x05));
    // From MIDI 2.0 it is a request.
    let (mut c, first) = Connection::with_wanted(wanted, t0);
    let mut peer = device();
    peer.reply(
        stream(Stream::StreamConfigurationRequest(StreamConfig {
            protocol: Protocol::Midi2,
            rx_jr: true,
            tx_jr: true,
        })),
        &mut Vec::<Ump>::new(),
    );
    let (_, sent) = run(&mut c, &mut peer, first, t0);
    assert_eq!(sent[1].0, [0xF005_0100, 0, 0, 0]);
    assert_eq!(c.endpoint().active(), wanted);
}

// A peer already in the wanted configuration is not asked again: what the
// Notification of Discovery reports is kept.
#[test]
fn a_peer_already_configured_is_not_asked() {
    let t0 = Instant::now();
    let (mut c, first) = Connection::new(t0);
    let mut peer = device();
    let configured = StreamConfig {
        protocol: Protocol::Midi2,
        rx_jr: true,
        tx_jr: true,
    };
    peer.reply(
        stream(Stream::StreamConfigurationRequest(configured)),
        &mut Vec::<Ump>::new(),
    );
    let (end, sent) = run(&mut c, &mut peer, first, t0);
    assert_eq!(end, Action::Settled(Ending::Negotiated));
    assert_eq!(sent.len(), 2);
    assert_eq!(c.endpoint().active(), configured);
}

fn notification(protocol: Protocol, rx_jr: bool, tx_jr: bool) -> Message {
    stream(Stream::StreamConfigurationNotification(StreamConfig {
        protocol,
        rx_jr,
        tx_jr,
    }))
}

/// A connection to a MIDI 2.0 capable peer with no blocks, fed its Discovery
/// replies by hand, so it has sent its Stream Configuration Request.
fn requested(t0: Instant) -> Connection {
    let (mut c, _) = Connection::new(t0);
    let mut peer = Endpoint::new(FULL);
    let mut replies: Vec<Ump> = Vec::new();
    peer.reply(
        stream(Stream::EndpointDiscovery {
            version: UMP_VERSION,
            filter: tutti_ump::DiscoveryFilter {
                endpoint_info: true,
                device_identity: false,
                endpoint_name: false,
                product_instance_id: false,
                stream_configuration: true,
            },
        }),
        &mut replies,
    );
    let actions: Vec<Action> = replies
        .into_iter()
        .map(|r| c.on_message(msg(r), t0))
        .collect();
    assert_eq!(actions[1], Action::Send(Ump([0xF005_0203, 0, 0, 0])));
    c
}

// §7.1.6.1 step 3: "If the Receiver is unable to change some or all parts of the
// configuration, the Receiver replies with its current configuration." The
// connection reports what the Notification says, not what it asked for.
#[test]
fn a_notification_of_another_protocol_is_reported_as_it_is() {
    let t0 = Instant::now();
    let mut c = requested(t0);
    let a = c.on_message(notification(Protocol::Midi1, false, true), at(t0, 5));
    assert_eq!(a, Action::Settled(Ending::Negotiated));
    assert_eq!(
        c.endpoint().active(),
        StreamConfig {
            protocol: Protocol::Midi1,
            rx_jr: false,
            tx_jr: true
        }
    );
}

#[test]
fn a_reserved_protocol_in_a_notification_is_midi1() {
    let t0 = Instant::now();
    let mut c = requested(t0);
    c.on_message(notification(Protocol::Reserved(9), true, true), t0);
    assert_eq!(c.endpoint().active().protocol, Protocol::Midi1);
}

// Only a Notification answers the Request (§7.1.6.2: not before it arrives).
#[test]
fn another_message_does_not_answer_the_request() {
    let t0 = Instant::now();
    let mut c = requested(t0);
    assert_eq!(c.on_message(stream(Stream::StartOfClip), t0), Action::Wait);
    assert_eq!(
        c.on_message(Message::Utility(tutti_ump::Utility::Noop), t0),
        Action::Wait
    );
    assert_eq!(c.settled(), None);
}

// No reply at all: sent again at each STEP_TIMEOUT, three attempts in all, then a
// MIDI 1.0 device with its one inferred block. Time is passed as values.
#[test]
fn a_silent_peer_settles_as_midi1_after_the_retries() {
    let t0 = Instant::now();
    let (mut c, first) = Connection::new(t0);
    assert_eq!(c.on_timeout(at(t0, 999)), Action::Wait);
    assert_eq!(c.on_timeout(at(t0, 1000)), Action::Send(first));
    assert_eq!(c.deadline(), Some(at(t0, 2000)));
    assert_eq!(c.on_timeout(at(t0, 1500)), Action::Wait);
    assert_eq!(c.on_timeout(at(t0, 2000)), Action::Send(first));
    assert_eq!(c.on_timeout(at(t0, 2999)), Action::Wait);
    assert_eq!(
        c.on_timeout(at(t0, 3000)),
        Action::Settled(Ending::Midi1Fallback)
    );
    assert_eq!(c.settled(), Some(Ending::Midi1Fallback));

    let e = c.endpoint();
    assert_eq!(e.active().protocol, Protocol::Midi1);
    assert!(!e.active().rx_jr && !e.active().tx_jr);
    assert!(e.capabilities().midi1 && !e.capabilities().midi2);
    assert_eq!(e.function_blocks().count(), 1);
    assert_eq!(
        e.function_block(id(0)).unwrap().midi1(),
        Midi1Port::Unrestricted
    );
    assert_eq!(c.on_timeout(at(t0, 9000)), Action::Wait);
}

// A reply to a retry is as good as to the first send.
#[test]
fn a_retry_that_is_answered_goes_on() {
    let t0 = Instant::now();
    let (mut c, first) = Connection::new(t0);
    let Action::Send(again) = c.on_timeout(at(t0, 1000)) else {
        panic!()
    };
    assert_eq!(again, first);
    let mut peer = device();
    let (end, _) = run(&mut c, &mut peer, again, at(t0, 1100));
    assert_eq!(end, Action::Settled(Ending::Negotiated));
}

// A peer that told us something but never its Stream Configuration is not one
// we know the protocol of: MIDI 1.0, keeping what it did say.
#[test]
fn a_peer_that_never_notifies_its_configuration_is_midi1() {
    let t0 = Instant::now();
    let (mut c, _) = Connection::new(t0);
    let mut peer = device();
    let mut replies: Vec<Ump> = Vec::new();
    peer.reply(
        stream(Stream::EndpointDiscovery {
            version: UMP_VERSION,
            filter: tutti_ump::DiscoveryFilter {
                endpoint_info: true,
                device_identity: false,
                endpoint_name: true,
                product_instance_id: false,
                stream_configuration: false,
            },
        }),
        &mut replies,
    );
    for r in replies {
        assert_eq!(c.on_message(msg(r), t0), Action::Wait);
    }
    c.on_timeout(at(t0, 1000));
    c.on_timeout(at(t0, 2000));
    assert_eq!(
        c.on_timeout(at(t0, 3000)),
        Action::Settled(Ending::Midi1Fallback)
    );
    assert_eq!(c.endpoint().active().protocol, Protocol::Midi1);
    assert_eq!(
        c.endpoint().name().unwrap().as_str(),
        "A long endpoint name of 40 characters!"
    );
}

// A Request nobody answers is also a MIDI 1.0 device, though the peer's
// capabilities say MIDI 2.0.
#[test]
fn an_unanswered_request_falls_back_to_midi1() {
    let t0 = Instant::now();
    let mut c = requested(t0);
    assert_eq!(
        c.on_timeout(at(t0, 1000)),
        Action::Send(Ump([0xF005_0203, 0, 0, 0]))
    );
    c.on_timeout(at(t0, 2000));
    assert_eq!(
        c.on_timeout(at(t0, 3000)),
        Action::Settled(Ending::Midi1Fallback)
    );
    assert_eq!(c.endpoint().active().protocol, Protocol::Midi1);
    assert!(c.endpoint().capabilities().midi2);
}

/// A connection that wants MIDI 1.0 from a peer that declares `n` blocks and
/// is in MIDI 1.0: it has gone straight to Function Block discovery.
fn in_blocks(t0: Instant, n: u8) -> Connection {
    let wanted = StreamConfig {
        protocol: Protocol::Midi1,
        rx_jr: false,
        tx_jr: false,
    };
    let (mut c, _) = Connection::with_wanted(wanted, t0);
    c.on_message(
        stream(Stream::EndpointInfo(tutti_ump::EndpointInfo {
            version: UMP_VERSION,
            static_function_blocks: true,
            function_blocks: U7::new(n).unwrap(),
            midi2: true,
            midi1: true,
            rx_jr: false,
            tx_jr: false,
        })),
        t0,
    );
    let a = c.on_message(notification(Protocol::Midi1, false, false), t0);
    assert_eq!(a, Action::Send(Ump([0xF010_FF03, 0, 0, 0])));
    c
}

fn info_of(n: u8) -> Message {
    stream(Stream::FunctionBlockInfo(FunctionBlockInfo {
        active: true,
        id: id(n),
        direction: BlockDirection::Input,
        ui_hint: UiHint::Receiver,
        midi1: Midi1Port::No,
        first_group: FirstGroup::new(n).unwrap(),
        groups_spanned: GroupSpan::new(1).unwrap(),
        midi_ci_version: 0,
        max_sysex8_streams: 0,
    }))
}

fn block_name_packet(n: u8, form: Form, text: &[u8]) -> Message {
    let mut bytes = [0u8; 13];
    bytes[..text.len()].copy_from_slice(text);
    stream(Stream::FunctionBlockName {
        block: id(n),
        form,
        bytes,
    })
}

// Function Blocks that do not all arrive: the protocol is settled, so the
// connection ends with the blocks it has after the retries, not as MIDI 1.0.
#[test]
fn missing_function_blocks_end_the_connection_with_what_came() {
    let t0 = Instant::now();
    let mut c = in_blocks(t0, 2);
    assert_eq!(c.on_message(info_of(0), at(t0, 20)), Action::Wait);
    // The Info of block 1 is missing: no grace, the step retries.
    assert_eq!(c.deadline(), Some(at(t0, 1000)));
    assert_eq!(
        c.on_timeout(at(t0, 1000)),
        Action::Send(Ump([0xF010_FF03, 0, 0, 0]))
    );
    c.on_timeout(at(t0, 2000));
    assert_eq!(
        c.on_timeout(at(t0, 3000)),
        Action::Settled(Ending::Negotiated)
    );
    assert_eq!(c.endpoint().active().protocol, Protocol::Midi1);
    assert_eq!(c.endpoint().function_blocks().count(), 1);
    assert_eq!(c.endpoint().declared_function_blocks(), 2);
}

// A Name that follows its block's Info within the grace is kept, and when every
// block has both the connection settles without waiting.
#[test]
fn a_name_within_the_grace_is_kept() {
    let t0 = Instant::now();
    let mut c = in_blocks(t0, 2);
    c.on_message(info_of(0), t0);
    c.on_message(info_of(1), at(t0, 5));
    assert_eq!(c.deadline(), Some(at(t0, 105)));
    assert_eq!(
        c.on_message(block_name_packet(0, Form::Complete, b"zero"), at(t0, 10)),
        Action::Wait
    );
    let a = c.on_message(block_name_packet(1, Form::Complete, b"one"), at(t0, 20));
    assert_eq!(a, Action::Settled(Ending::Negotiated));
    assert_eq!(
        c.endpoint()
            .function_block(id(0))
            .unwrap()
            .name()
            .unwrap()
            .as_str(),
        "zero"
    );
    assert_eq!(
        c.endpoint()
            .function_block(id(1))
            .unwrap()
            .name()
            .unwrap()
            .as_str(),
        "one"
    );
}

// A reply after the connection settled changes nothing.
#[test]
fn a_reply_after_settling_is_ignored() {
    let t0 = Instant::now();
    let (mut c, first) = Connection::new(t0);
    c.on_timeout(at(t0, 1000));
    c.on_timeout(at(t0, 2000));
    c.on_timeout(at(t0, 3000));
    let before = *c.endpoint();
    let mut peer = device();
    let mut replies: Vec<Ump> = Vec::new();
    peer.reply(msg(first), &mut replies);
    for r in replies {
        assert_eq!(c.on_message(msg(r), at(t0, 3100)), Action::Wait);
    }
    assert_eq!(*c.endpoint(), before);
    assert_eq!(c.settled(), Some(Ending::Midi1Fallback));
}

// A settled-by-success connection ignores later messages too, including a
// second Notification.
#[test]
fn a_notification_after_a_negotiated_settle_is_ignored() {
    let t0 = Instant::now();
    let (mut c, first) = Connection::new(t0);
    let mut peer = Endpoint::midi1_device(None);
    run(&mut c, &mut peer, first, t0);
    assert_eq!(
        c.on_timeout(at(t0, 100)),
        Action::Settled(Ending::Negotiated)
    );
    let before = *c.endpoint();
    assert_eq!(
        c.on_message(notification(Protocol::Midi2, true, true), t0),
        Action::Wait
    );
    assert_eq!(*c.endpoint(), before);
}

fn name_packet(form: Form, text: &[u8]) -> Message {
    let mut b = [0u8; 14];
    b[..text.len()].copy_from_slice(text);
    stream(Stream::EndpointName(form, b))
}

// §7.1.4: a name arrives as a Start, Continues and an End. A Continue or End with
// no Start before it, and a Start that is interrupted by another, do not make a
// name.
#[test]
fn names_are_reassembled_from_several_packets() {
    let t0 = Instant::now();
    let (mut c, _) = Connection::new(t0);
    c.on_message(name_packet(Form::End, b"orphan"), t0);
    assert!(c.endpoint().name().is_none());
    c.on_message(name_packet(Form::Start, b"abandoned name"), t0);
    c.on_message(name_packet(Form::Start, b"first, abcdef "), t0);
    c.on_message(name_packet(Form::Continue, b"second part!!!"), t0);
    assert!(c.endpoint().name().is_none());
    c.on_message(name_packet(Form::End, b"end"), t0);
    assert_eq!(
        c.endpoint().name().unwrap().as_str(),
        "first, abcdef second part!!!end"
    );
}
// A Function Block Name of one block is not finished by the packets of another:
// one name is open at a time.
#[test]
fn block_names_of_two_blocks_do_not_mix() {
    let t0 = Instant::now();
    let mut c = in_blocks(t0, 2);
    c.on_message(info_of(0), t0);
    c.on_message(info_of(1), t0);
    c.on_message(block_name_packet(0, Form::Start, b"0123456789abc"), t0);
    c.on_message(block_name_packet(1, Form::Start, b"ABCDEFGHIJKLM"), t0);
    c.on_message(block_name_packet(0, Form::End, b"def"), t0);
    c.on_message(block_name_packet(1, Form::End, b"end"), t0);
    assert_eq!(
        c.on_timeout(at(t0, 100)),
        Action::Settled(Ending::Negotiated)
    );
    let name = |n| {
        c.endpoint()
            .function_block(id(n))
            .unwrap()
            .name()
            .map(BlockName::as_str)
    };
    assert_eq!(name(0), None);
    assert_eq!(name(1), Some("ABCDEFGHIJKLMend"));
}
