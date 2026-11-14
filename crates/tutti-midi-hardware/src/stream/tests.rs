//! The UMP Stream negotiation against a fake device that answers as
//! M2-104 §7.1 says an Endpoint does (`Endpoint::reply`), through the session
//! and the fake backend: the wire is the fake's sink and the input's
//! producer.

use super::*;
use crate::MidiSession;
use crate::session::Polled;
use crate::test_support::{FakeBackend, FakeCounters, endpoint};
use crate::{HardwareMidiInputs, Pushed};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;
use tutti_fixtures::midi;
use tutti_graph::UmpAt;
use tutti_midi_device::{
    BlockName, Capabilities, DeviceId, EndpointId, FunctionBlock, MAX_ATTEMPTS, PortDirection,
    STEP_TIMEOUT,
};
use tutti_ump::ControlValue;
use tutti_ump::MidiChannel;
use tutti_ump::{
    Direction as BlockDirection, FirstGroup, FunctionBlockId, GroupSpan, Message, Stream,
    SysexAssembler, SysexStep,
};

/// A fake duplex device `raw` and the session that opened its input.
struct Wire {
    session: MidiSession,
    /// The output the caller holds, following what the session polls.
    output: Option<OpenOutput>,
    counters: FakeCounters,
    id: DeviceId,
    raw: EndpointId,
    /// The device's own state, which answers what it is sent.
    device: Endpoint,
    start: Instant,
}

/// The stream reports among what a poll returned.
fn streams(polled: impl IntoIterator<Item = Polled>) -> Vec<(DeviceId, DeviceStream)> {
    polled
        .into_iter()
        .filter_map(|p| match p {
            Polled::Stream(id, stream) => Some((id, *stream)),
            _ => None,
        })
        .collect()
}

fn session_over(os: UmpCapability, direction: PortDirection, device: Endpoint) -> Wire {
    let (backend, counters) = FakeBackend::build_with(vec![], vec![], usize::MAX);
    let inlets = tutti_midi_runtime::DeviceInlets::new(
        NonZeroUsize::new(4).unwrap(),
        tutti_graph::EventCapacity::of(64),
        Duration::ZERO,
    );
    let session = MidiSession::open_over(backend, inlets).unwrap();
    let mut info = endpoint(7, "Wire");
    info.capability = os;
    counters.plug(info, direction);
    let id = session
        .inputs()
        .first()
        .map(|d| d.id.clone())
        .unwrap_or_else(|| session.outputs().first().expect("the device").id.clone());
    Wire {
        session,
        output: None,
        counters,
        id,
        raw: EndpointId::from_raw(7),
        device,
        start: Instant::now(),
    }
}

impl Wire {
    fn at(&self, ms: u64) -> Instant {
        self.start + Duration::from_millis(ms)
    }

    /// A poll at `ms`, whose reports the held output follows.
    fn poll(&mut self, ms: u64) -> Vec<Polled> {
        let polled: Vec<_> = self.session.poll(self.at(ms)).collect();
        if let Some(output) = &mut self.output {
            for report in &polled {
                output.follow(report);
            }
        }
        polled
    }

    /// Connects the output of the device, held by the wire.
    fn connect_output(&mut self) {
        self.output = Some(self.session.connect_output(&self.id).unwrap());
    }

    /// Queues `events` to the held output: how many the device took.
    fn send(&mut self, events: &[UmpAt]) -> usize {
        self.output.as_mut().expect("an output").queue(events)
    }

    /// One tick of the session at `ms`, then the device reads what the session
    /// sent and answers into the input. Returns what the session sent, and the
    /// devices the tick settled.
    fn tick(&mut self, ms: u64) -> (Vec<Stream>, Vec<(DeviceId, DeviceStream)>) {
        let now = self.at(ms);
        let settled = streams(self.poll(ms));
        let sent: Vec<Stream> = self
            .counters
            .take_sent(self.raw)
            .into_iter()
            // MIDI-CI is SysEx and is not the negotiation's.
            .filter(|ump| ump.message_type() != 3)
            .map(|ump| match ump.decode() {
                Ok(Message::Stream(stream)) => stream,
                other => panic!("sent something that is not a Stream message: {other:?}"),
            })
            .collect();
        let mut answers = Vec::new();
        for stream in &sent {
            self.device.reply(Message::Stream(*stream), &mut answers);
        }
        for ump in answers {
            assert!(self.counters.send_input(self.raw, ump, now));
        }
        (sent, settled)
    }
}

fn block() -> FunctionBlock {
    FunctionBlock::builder(
        FunctionBlockId::new(0).unwrap(),
        BlockDirection::Bidirectional,
        FirstGroup::new(0).unwrap(),
        GroupSpan::new(2).unwrap(),
    )
    .name(BlockName::new("Main").unwrap())
    .build()
    .unwrap()
}

/// A device that offers both protocols and JR both ways, in MIDI 1.0 now.
fn offering_both() -> Endpoint {
    Endpoint::new(Capabilities {
        midi1: true,
        midi2: true,
        rx_jr: true,
        tx_jr: true,
    })
    .with_function_block(block())
}

/// M2-100 Table 4 steps 2 to 4 over the wire: Endpoint Discovery first, then
/// (the device is in MIDI 1.0 and offers MIDI 2.0) a Stream Configuration
/// Request for the MIDI 2.0 Protocol, then Function Block Discovery; and the
/// stream settles on what the device's Notification said. **JR Timestamps stay
/// off though the device offers them** (the module's reasoning).
#[test]
fn a_device_that_answers_is_negotiated_onto_midi2_with_jr_off() {
    let mut wire = session_over(
        UmpCapability::midi1(),
        PortDirection::Duplex,
        offering_both(),
    );
    // The OS says MIDI 1.0 with no blocks: not a UMP Endpoint, nothing asked.
    wire.session.connect_input(&wire.id.clone()).unwrap();
    assert_eq!(
        wire.session.stream(&wire.id).unwrap().basis(),
        Basis::Described
    );

    let mut wire = session_over(
        UmpCapability::midi2(),
        PortDirection::Duplex,
        offering_both(),
    );
    wire.session.connect_input(&wire.id.clone()).unwrap();
    assert_eq!(
        wire.session.stream(&wire.id),
        None,
        "asked, not yet answered"
    );

    let (first, _) = wire.tick(0);
    assert!(
        matches!(first.as_slice(), [Stream::EndpointDiscovery { filter, .. }]
            if filter.endpoint_info && filter.stream_configuration),
        "{first:?}"
    );
    let (second, _) = wire.tick(1);
    assert!(
        matches!(
            second.as_slice(),
            [Stream::StreamConfigurationRequest(StreamConfig {
                protocol: Protocol::Midi2,
                rx_jr: false,
                tx_jr: false,
            })]
        ),
        "{second:?}"
    );
    let (third, _) = wire.tick(2);
    assert!(
        matches!(third.as_slice(), [Stream::FunctionBlockDiscovery { .. }]),
        "{third:?}"
    );
    let (_, settled) = wire.tick(3);
    let [(id, stream)] = settled.as_slice() else {
        panic!("settled: {settled:?}");
    };
    assert_eq!(*id, wire.id);
    assert_eq!(stream.basis(), Basis::Negotiated);
    assert_eq!(
        stream.config(),
        StreamConfig {
            protocol: Protocol::Midi2,
            rx_jr: false,
            tx_jr: false
        }
    );
    assert_eq!(wire.session.stream(&wire.id), Some(*stream));
    let blocks = stream.capability().blocks;
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].name().map(BlockName::as_str), Some("Main"));
    assert_eq!(blocks[0].groups().get(), 2);
    // Settled: nothing more is sent, nothing more settles.
    let (more, settled) = wire.tick(10_000);
    assert!(more.is_empty() && settled.is_empty());
}

/// A device that offers MIDI 1.0 only settles in MIDI 1.0, and no request is
/// sent for a protocol it does not have (§7.1.2: only what the Endpoint
/// supports).
#[test]
fn a_device_that_offers_only_midi1_stays_in_midi1() {
    let device = Endpoint::new(Capabilities {
        midi1: true,
        ..Capabilities::default()
    })
    .with_function_block(block());
    let mut wire = session_over(UmpCapability::midi2(), PortDirection::Duplex, device);
    wire.session.connect_input(&wire.id.clone()).unwrap();
    let mut all = Vec::new();
    let mut settled = Vec::new();
    for ms in 0..4 {
        let (sent, done) = wire.tick(ms);
        all.extend(sent);
        settled.extend(done);
    }
    assert!(
        !all.iter()
            .any(|s| matches!(s, Stream::StreamConfigurationRequest(_))),
        "{all:?}"
    );
    let [(_, stream)] = settled.as_slice() else {
        panic!("settled: {settled:?}");
    };
    assert_eq!(stream.basis(), Basis::Negotiated);
    assert_eq!(stream.config().protocol, Protocol::Midi1);
}

/// M2-104 Appendix I: a device that does not answer UMP Stream messages never
/// hangs the session. Every `poll` returns at once; the Endpoint Discovery is
/// sent `MAX_ATTEMPTS` times, a step apart, and after the last wait the device
/// is `Unanswered`, as the OS describes it (here MIDI 2.0, what the OS said).
#[test]
fn a_silent_device_settles_as_the_os_describes_it() {
    let silent = Endpoint::new(Capabilities::default());
    let mut wire = session_over(UmpCapability::midi2(), PortDirection::Duplex, silent);
    wire.session.connect_input(&wire.id.clone()).unwrap();

    let step = STEP_TIMEOUT.as_millis() as u64;
    let mut sent = 0;
    let mut settled = Vec::new();
    // Answers are dropped on the floor: this device never replies.
    for ms in [
        0,
        10,
        step / 2,
        step,
        step + 10,
        2 * step,
        3 * step - 1,
        3 * step + 1,
    ] {
        let now = wire.at(ms);
        settled.extend(streams(wire.session.poll(now)));
        sent += wire.counters.take_sent(wire.raw).len();
    }
    assert_eq!(sent, usize::from(MAX_ATTEMPTS), "one send per attempt");
    let [(_, stream)] = settled.as_slice() else {
        panic!("settled: {settled:?}");
    };
    assert_eq!(stream.basis(), Basis::Unanswered);
    assert_eq!(stream.config().protocol, Protocol::Midi2, "the OS's word");
    assert!(!stream.config().rx_jr && !stream.config().tx_jr);
}

/// An OS that says MIDI 1.0 and lists no blocks has no UMP Stream to ask: no
/// Stream message is sent, and the device is `Described` from the connect (no
/// wait). (It is sent an Identity Request, the tests below.)
#[test]
fn a_midi1_port_is_not_sent_stream_messages_and_is_described_at_once() {
    let mut wire = session_over(
        UmpCapability::midi1(),
        PortDirection::Duplex,
        Endpoint::new(Capabilities::default()),
    );
    wire.session.connect_input(&wire.id.clone()).unwrap();
    let stream = wire.session.stream(&wire.id).expect("settled at connect");
    assert_eq!(stream.basis(), Basis::Described);
    assert_eq!(stream.config().protocol, Protocol::Midi1);
    let settled: Vec<_> = wire.session.poll(wire.at(0)).collect();
    assert!(settled.is_empty());
    let sent = received(&wire);
    assert!(
        sent.iter().all(|ump| ump.message_type() == 3),
        "only SysEx goes to a MIDI 1.0 port, got {sent:?}"
    );
}

// --- The Identity Request of a MIDI 1.0 port -------------------------------

/// A MIDI 1.0 duplex device, connected, and what the session sent at `ms`.
fn midi1_wire() -> Wire {
    let mut wire = session_over(
        UmpCapability::midi1(),
        PortDirection::Duplex,
        Endpoint::new(Capabilities::default()),
    );
    wire.session.connect_input(&wire.id.clone()).unwrap();
    wire
}

/// The SysEx7 packets of the payload between `F0` and `F7`.
fn sysex(payload: &[u8]) -> Vec<Ump> {
    tutti_ump::Sysex7Packets::from_payload(tutti_ump::MidiGroup::FIRST, payload)
        .expect("7-bit payload")
        .collect()
}

/// M1 p.40: `F0 7E <device ID> 06 02 mm ff ff dd dd ss ss ss ss F7`.
fn identity_reply(device: u8) -> Vec<Ump> {
    sysex(&[
        0x7E, device, 0x06, 0x02, 0x41, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
    ])
}

fn identity() -> tutti_ump::DeviceIdentity {
    use tutti_ump::{ManufacturerId, U7};
    tutti_ump::DeviceIdentity {
        manufacturer: ManufacturerId::new(0x41).unwrap(),
        family: [U7::of(1), U7::of(2)],
        model: [U7::of(3), U7::of(4)],
        revision: [U7::of(5), U7::of(6), U7::of(7), U7::of(8)],
    }
}

fn reply_at(wire: &Wire, packets: Vec<Ump>, ms: u64) {
    for ump in packets {
        assert!(wire.counters.send_input(wire.raw, ump, wire.at(ms)));
    }
}

/// The first poll of a MIDI 1.0 port with an output sends the all-call Identity
/// Request (M1 p.40) as one SysEx7 packet; a device that answers, whatever its
/// own device id, gives the Endpoint its identity, which `poll` reports as an
/// `Inquired` stream and `stream` then holds.
#[test]
fn a_midi1_device_that_answers_an_identity_request_has_its_identity() {
    let mut wire = midi1_wire();
    assert!(wire.session.poll(wire.at(0)).next().is_none());
    assert_eq!(received(&wire), sysex(&[0x7E, 0x7F, 0x06, 0x01]));
    assert_eq!(
        wire.session.stream(&wire.id).unwrap().endpoint().identity(),
        None
    );

    // The device's own id is 5, not the all-call.
    reply_at(&wire, identity_reply(0x05), 10);
    let settled = streams(wire.session.poll(wire.at(20)));
    let [(id, stream)] = settled.as_slice() else {
        panic!("the answer is reported once, got {settled:?}");
    };
    assert_eq!(*id, wire.id);
    assert_eq!(stream.basis(), Basis::Inquired);
    assert_eq!(stream.endpoint().identity(), Some(&identity()));
    assert_eq!(stream.config().protocol, Protocol::Midi1);
    assert_eq!(
        wire.session.stream(&wire.id).unwrap().endpoint().identity(),
        Some(&identity())
    );
    // Over: no Identity Request is sent again, however long it goes on (what
    // is sent is MIDI-CI's, which takes the SysEx reader from here).
    for ms in [30, 5000, 10_000] {
        let _ = wire.session.poll(wire.at(ms)).count();
        assert_eq!(identity_requests(&received(&wire)), 0);
    }
}

/// A device that never answers (one that does not implement Device Inquiry) is
/// sent the request twice, one timeout apart, and then left alone: it is the
/// `Described` device it was, with no identity, and nothing waits.
#[test]
fn a_silent_midi1_device_is_asked_twice_and_stays_described() {
    use tutti_midi_device::{INQUIRY_ATTEMPTS, INQUIRY_TIMEOUT};
    let mut wire = midi1_wire();
    let ms = INQUIRY_TIMEOUT.as_millis() as u64;
    let mut sends = 0;
    for step in 0..6 {
        assert!(wire.session.poll(wire.at(step * ms)).next().is_none());
        sends += identity_requests(&received(&wire));
    }
    assert_eq!(sends, usize::from(INQUIRY_ATTEMPTS));
    let stream = wire.session.stream(&wire.id).unwrap();
    assert_eq!(stream.basis(), Basis::Described);
    assert_eq!(stream.endpoint().identity(), None);
}

/// Once the inquiry has given up on a silent device, its copy of the device's
/// SysEx7 packets is off (a packet that arrives is not kept in the stream ring,
/// though it still reaches the graph, the next test) and nothing more is sent.
#[test]
fn after_giving_up_the_sysex_copy_is_off_and_nothing_is_sent() {
    use tutti_midi_device::{INQUIRY_ATTEMPTS, INQUIRY_TIMEOUT};
    let mut wire = midi1_wire();
    let ms = INQUIRY_TIMEOUT.as_millis() as u64;
    // The request, its one retry, then the poll that gives up.
    for step in 0..=u64::from(INQUIRY_ATTEMPTS) {
        assert!(wire.session.poll(wire.at(step * ms)).next().is_none());
    }
    let slot = wire.session.ports().list_input_ports()[0].id;
    let _ = received(&wire);
    // Whatever the ring holds from the inquiry is read off.
    let _ = wire.session.ports().drain_stream(slot).count();

    reply_at(&wire, identity_reply(0), 10 * ms);
    assert_eq!(
        wire.session.ports().drain_stream(slot).count(),
        0,
        "the copy is off once the inquiry is over"
    );
    for step in 10..13 {
        assert!(wire.session.poll(wire.at(step * ms)).next().is_none());
    }
    assert!(
        received(&wire).is_empty(),
        "nothing is sent after giving up"
    );
}

/// An output the session cannot open (another app holds it) does not fail the
/// input: the connect succeeds, the device is the `Described` device the OS
/// says it is, and nothing is sent or waited for.
#[test]
fn a_refused_output_does_not_fail_the_input_and_nothing_is_asked() {
    use tutti_midi_device::INQUIRY_TIMEOUT;
    let mut wire = session_over(
        UmpCapability::midi1(),
        PortDirection::Duplex,
        Endpoint::new(Capabilities::default()),
    );
    wire.counters.refuse_output_opens(true);
    wire.session.connect_input(&wire.id.clone()).unwrap();
    assert!(wire.session.is_input_connected(&wire.id));
    let ms = INQUIRY_TIMEOUT.as_millis() as u64;
    for step in 0..4 {
        assert!(wire.session.poll(wire.at(step * ms)).next().is_none());
    }
    assert!(received(&wire).is_empty(), "no output, nothing sent");
    let stream = wire.session.stream(&wire.id).unwrap();
    assert_eq!(stream.basis(), Basis::Described);
    assert_eq!(stream.endpoint().identity(), None);
}

/// The output an input opens to ask its device is that input's own: it is not
/// one the caller holds (`connect_output` to the same device is a separate open
/// that the caller owns), and it goes with the input: once disconnected, the
/// retry the inquiry would have sent is never sent.
#[test]
fn the_asking_output_is_the_inputs_own_and_closes_with_it() {
    use tutti_midi_device::INQUIRY_TIMEOUT;
    let mut wire = midi1_wire();
    assert!(wire.session.poll(wire.at(0)).next().is_none());
    assert_eq!(received(&wire).len(), 1, "the request");

    assert!(wire.session.disconnect_input(&wire.id.clone()));
    let ms = INQUIRY_TIMEOUT.as_millis() as u64;
    for step in 1..4 {
        assert!(wire.session.poll(wire.at(step * ms)).next().is_none());
    }
    assert!(received(&wire).is_empty(), "no retry after the input went");
    assert!(wire.session.stream(&wire.id).is_none());

    let output = wire.session.connect_output(&wire.id.clone()).unwrap();
    assert_eq!(output.id(), &wire.id);
}

/// A gap in the stream ring never joins an Identity Reply. The message sent is
/// 21 bytes: the first six and the last nine of a valid reply with a packet of
/// six between them. The ring loses that packet, so joined, what is left is a
/// valid 15-byte reply (and would be taken); it is no reply, and a whole one
/// after it answers.
#[test]
fn a_gap_in_the_stream_ring_never_joins_an_identity_reply() {
    let mut wire = midi1_wire();
    assert!(wire.session.poll(wire.at(0)).next().is_none());
    let valid = [
        0x7E, 0x7F, 0x06, 0x02, 0x41, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
    ];
    let payload: Vec<u8> = [&valid[..6], &[0x11; 6], &valid[6..]].concat();
    let packets: Vec<Ump> =
        tutti_ump::Sysex7Packets::from_payload(tutti_ump::MidiGroup::FIRST, &payload)
            .unwrap()
            .collect();
    assert_eq!(packets.len(), 4, "Start, Continue, Continue, End");
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
    // The Start, then the ring fills to overflowing: the first Continue is lost.
    reply_at(&wire, vec![packets[0]], 1);
    for _ in 0..crate::port::async_port::STREAM_CAPACITY + 8 {
        reply_at(&wire, vec![filler], 2);
    }
    reply_at(&wire, vec![packets[1]], 3);
    assert!(wire.session.poll(wire.at(4)).next().is_none());
    reply_at(&wire, packets[2..].to_vec(), 5);
    assert!(wire.session.poll(wire.at(6)).next().is_none());
    assert_eq!(
        wire.session.stream(&wire.id).unwrap().basis(),
        Basis::Described,
        "a reply with a hole is no reply"
    );
    reply_at(&wire, identity_reply(0), 7);
    let answered = streams(wire.session.poll(wire.at(8)));
    assert!(matches!(answered.as_slice(), [(_, s)] if s.basis() == Basis::Inquired));
}

/// A late reply (after the inquiry gave up) and a SysEx that is not an Identity
/// Reply (here General MIDI System On, M1 p.52) leave the device as it was.
#[test]
fn only_an_identity_reply_inside_the_inquiry_answers_it() {
    use tutti_midi_device::INQUIRY_TIMEOUT;
    let mut wire = midi1_wire();
    let ms = INQUIRY_TIMEOUT.as_millis() as u64;
    assert!(wire.session.poll(wire.at(0)).next().is_none());
    reply_at(&wire, sysex(&[0x7E, 0x7F, 0x09, 0x01]), 1);
    assert!(wire.session.poll(wire.at(2)).next().is_none());
    assert_eq!(
        wire.session.stream(&wire.id).unwrap().endpoint().identity(),
        None
    );
    for step in 1..4 {
        assert!(wire.session.poll(wire.at(step * ms)).next().is_none());
    }
    reply_at(&wire, identity_reply(0), 4 * ms);
    assert!(wire.session.poll(wire.at(4 * ms + 1)).next().is_none());
    assert_eq!(
        wire.session.stream(&wire.id).unwrap().basis(),
        Basis::Described
    );
}

/// While the request is out, the input edge keeps a copy of the device's SysEx7
/// packets for the control side, and the graph still hears them (a patch dump
/// must not be eaten by an inquiry). With the copy off, nothing is kept.
#[test]
fn sysex_is_copied_to_the_stream_ring_while_asked_and_always_reaches_the_graph() {
    use crate::testing::Rig;

    let inlets = tutti_midi_runtime::DeviceInlets::new(
        NonZeroUsize::new(2).unwrap(),
        tutti_graph::EventCapacity::of(64),
        Duration::ZERO,
    );
    let ports = Arc::new(HardwareMidiInputs::new(Arc::clone(&inlets)));
    let mut rig = Rig::over(&inlets, Instant::now());
    rig.run(1);
    let mut producer = ports.open_input_port("dev").unwrap();
    let port = producer.slot();
    rig.apply();
    let packets = identity_reply(0);
    let arrival = rig.origin + Duration::from_millis(1);

    assert!(ports.hear_sysex(port, true));
    for ump in &packets {
        assert_eq!(producer.push(*ump, arrival), Pushed::Sent);
    }
    assert_eq!(ports.drain_stream(port).collect::<Vec<_>>(), packets);

    assert!(ports.hear_sysex(port, false));
    for ump in &packets {
        assert_eq!(producer.push(*ump, arrival), Pushed::Sent);
    }
    assert_eq!(ports.drain_stream(port).count(), 0);

    rig.run(8);
    let heard: Vec<_> = rig.take().into_iter().map(|(_, _, ump)| ump).collect();
    assert_eq!(heard, [packets.clone(), packets].concat());
}

/// A UMP Endpoint the session can only read (no output) cannot be asked:
/// `Described`, as the OS says, with no wait.
#[test]
fn an_endpoint_with_no_output_is_described_not_waited_for() {
    let mut wire = session_over(
        UmpCapability::midi2(),
        PortDirection::Input,
        Endpoint::new(Capabilities::default()),
    );
    wire.session.connect_input(&wire.id.clone()).unwrap();
    let stream = wire.session.stream(&wire.id).expect("settled at connect");
    assert_eq!(stream.basis(), Basis::Described);
    assert_eq!(stream.config().protocol, Protocol::Midi2);
}

/// A device's Stream messages are not music: they reach the stream ring and
/// never the graph's inlet, while a note beside them does.
#[test]
fn stream_messages_never_reach_the_graph() {
    use crate::testing::Rig;

    let inlets = tutti_midi_runtime::DeviceInlets::new(
        NonZeroUsize::new(2).unwrap(),
        tutti_graph::EventCapacity::of(64),
        Duration::ZERO,
    );
    let ports = Arc::new(HardwareMidiInputs::new(Arc::clone(&inlets)));
    let start = Instant::now();
    let mut rig = Rig::over(&inlets, start);
    rig.run(1);
    let mut producer = ports.open_input_port("dev").unwrap();
    let port = producer.slot();
    rig.apply();
    let stream = tutti_ump::Ump::from(Message::Stream(Stream::EndpointDiscovery {
        version: tutti_midi_device::UMP_VERSION,
        filter: tutti_ump::DiscoveryFilter {
            endpoint_info: true,
            device_identity: false,
            endpoint_name: false,
            product_instance_id: false,
            stream_configuration: false,
        },
    }));
    let note = midi::note_on(MidiChannel::FIRST, 60, 0x8000);
    let arrival = rig.origin + Duration::from_millis(1);
    assert_eq!(producer.push(stream, arrival), Pushed::Diverted);
    assert_eq!(producer.push(note, arrival), Pushed::Sent);
    rig.run(8);
    let heard: Vec<_> = rig.take().into_iter().map(|(_, _, ump)| ump).collect();
    assert_eq!(heard, [note]);
    let said: Vec<_> = ports.drain_stream(port).collect();
    assert_eq!(said, [stream]);
}

// --- The output follows what the device said -------------------------------

/// What the fake device received on its output, as the packets.
fn received(wire: &Wire) -> Vec<Ump> {
    wire.counters.take_sent(wire.raw)
}

/// The complete System Exclusive messages among `packets`, each with its Group
/// and its payload (between `F0` and `F7`).
fn payloads(packets: &[Ump]) -> Vec<(tutti_ump::MidiGroup, Vec<u8>)> {
    let room = SysexAssembler::room_for(256, 1, 16);
    let mut assembler = SysexAssembler::new(vec![0; room].into_boxed_slice(), 256, 16);
    let mut found = Vec::new();
    for &ump in packets {
        assembler.begin_block();
        if let SysexStep::Complete(done) = assembler.push(ump) {
            let group = ump.group().expect("a SysEx7 packet has a Group");
            found.push((group, assembler.message(done).payload().to_vec()));
        }
    }
    found
}

/// How many Identity Requests (M1 p.40) `packets` hold.
fn identity_requests(packets: &[Ump]) -> usize {
    payloads(packets)
        .iter()
        .filter(|(_, payload)| payload == &[0x7E, 0x7F, 0x06, 0x01])
        .count()
}

fn send_ump(wire: &Wire, stream: Stream, ms: u64) {
    let ump = Ump::from(Message::Stream(stream));
    assert!(wire.counters.send_input(wire.raw, ump, wire.at(ms)));
}

fn notification(protocol: Protocol, rx_jr: bool) -> Stream {
    Stream::StreamConfigurationNotification(StreamConfig {
        protocol,
        rx_jr,
        tx_jr: false,
    })
}

/// Ticks until the stream settles; the device's answers are the fake's.
fn settle(wire: &mut Wire) -> DeviceStream {
    for ms in 0..8 {
        if let Some((_, stream)) = wire.tick(ms).1.pop() {
            return stream;
        }
    }
    panic!("the negotiation did not settle");
}

fn rpn_ump(value: u32) -> Ump {
    use tutti_ump::{Address, U7, Voice2};
    Ump::from(Message::Midi2(
        Address::first(tutti_ump::MidiChannel::FIRST),
        Voice2::Registered {
            bank: U7::of(0),
            index: U7::of(0),
            value: ControlValue::new(value),
        },
    ))
}

/// M2-104 §3.2: a MIDI 1.0 endpoint uses no message of Message
/// Type 0x4. A device the OS describes as a MIDI 1.0 port (no UMP Stream is
/// asked of it) is sent only type 2 whatever the graph carries, and a
/// Registered Controller arrives as the Control Changes 101, 100, 6 and 38 of
/// Appendix D.2.3.
/// Spec: m2-104/3.2#3
#[test]
fn a_midi1_port_is_never_sent_type_4() {
    let mut wire = session_over(
        UmpCapability::midi1(),
        PortDirection::Output,
        offering_both(),
    );
    wire.connect_output();
    let note = midi::note_on(MidiChannel::FIRST, 60, 0xFFFF);
    assert_eq!(
        wire.send(&[(Offset::ZERO, note), (Offset::ZERO, rpn_ump(12 << 25))]),
        2
    );
    let got = received(&wire);
    assert!(got.iter().all(|u| u.message_type() == 2), "{got:?}");
    let bytes: Vec<(u8, u8)> = got[1..]
        .iter()
        .map(|&u| match u.decode() {
            Ok(Message::Midi1(_, tutti_ump::Voice1::ControlChange { controller, value })) => {
                (controller.get(), value.get())
            }
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(bytes, [(101, 0), (100, 0), (6, 12), (38, 0)]);
}

/// The output sends in what the negotiation settled, whenever it settles and
/// whichever came first: an output connected before the settle is moved by it
/// (the OS said MIDI 2.0, the device is MIDI 1.0), and a later Stream
/// Configuration Notification moves it again.
#[test]
fn the_output_follows_the_negotiated_protocol() {
    let device = Endpoint::new(Capabilities {
        midi1: true,
        midi2: true,
        ..Capabilities::default()
    })
    .with_function_block(block());
    // A device that has MIDI 2.0 on offer but is in MIDI 1.0; the session asks
    // for MIDI 2.0, so it ends in MIDI 2.0 ...
    let mut wire = session_over(UmpCapability::midi2(), PortDirection::Duplex, device);
    wire.session.connect_input(&wire.id.clone()).unwrap();
    wire.connect_output();
    let note = [(Offset::ZERO, midi::note_on(MidiChannel::FIRST, 60, 0xFFFF))];
    // ... but until then the output is what the OS said: MIDI 2.0.
    wire.send(&note);
    assert_eq!(received(&wire)[0].message_type(), 4);

    let stream = settle(&mut wire);
    assert_eq!(stream.config().protocol, Protocol::Midi2);
    received(&wire);
    wire.send(&note);
    assert_eq!(received(&wire)[0].message_type(), 4);

    // The device goes to MIDI 1.0 on its own: it is told, and the output goes
    // with it.
    send_ump(&wire, notification(Protocol::Midi1, false), 20);
    let changed = streams(wire.poll(20));
    assert!(
        matches!(changed.as_slice(), [(_, s)] if s.config().protocol == Protocol::Midi1),
        "{changed:?}"
    );
    wire.send(&note);
    assert_eq!(received(&wire)[0].message_type(), 2);
    assert_eq!(
        wire.output.as_ref().map(|o| o.config().protocol),
        Some(Protocol::Midi1)
    );
}

/// M2-104 §7.1.6.2: JR Timestamps are sent only after a Stream Configuration
/// Notification has been received. The session asks for none, so a JR
/// Clock the output is handed does not reach the device; after the device
/// notifies that it receives them, it does.
/// Spec: m2-104/7.1.6.2#3
#[test]
fn jr_reaches_a_device_only_after_its_notification_enables_it() {
    let mut wire = session_over(
        UmpCapability::midi2(),
        PortDirection::Duplex,
        offering_both(),
    );
    wire.session.connect_input(&wire.id.clone()).unwrap();
    let stream = settle(&mut wire);
    assert!(!stream.config().rx_jr);
    wire.connect_output();
    let clock = Ump::from(Message::Utility(tutti_ump::Utility::JrClock(7)));
    let note = midi::note_on(tutti_ump::MidiChannel::FIRST, 60, 0xFFFF);
    let events = [(Offset::ZERO, clock), (Offset::ZERO, note)];
    received(&wire);
    assert_eq!(wire.send(&events), 2, "nothing was refused");
    assert_eq!(received(&wire), [note]);

    send_ump(&wire, notification(Protocol::Midi2, true), 30);
    assert_eq!(wire.poll(30).len(), 1);
    wire.send(&events);
    assert_eq!(received(&wire), [clock, note]);
}

fn moved_block(first_group: u8) -> Stream {
    Stream::FunctionBlockInfo(tutti_ump::FunctionBlockInfo {
        active: true,
        id: FunctionBlockId::new(0).unwrap(),
        direction: BlockDirection::Bidirectional,
        ui_hint: tutti_ump::UiHint::Both,
        midi1: tutti_ump::Midi1Port::No,
        first_group: FirstGroup::new(first_group).unwrap(),
        groups_spanned: GroupSpan::new(2).unwrap(),
        midi_ci_version: 0,
        max_sysex8_streams: 0,
    })
}

/// M2-104 §6.1.1: an Endpoint that declared its blocks static does not change
/// them after discovery, so a Function Block Info it sends then is not read;
/// one that did not is followed, and `poll` says so.
#[test]
fn static_blocks_are_not_re_read_and_others_are() {
    for is_static in [true, false] {
        let device = offering_both().with_static_function_blocks(is_static);
        let mut wire = session_over(UmpCapability::midi2(), PortDirection::Duplex, device);
        wire.session.connect_input(&wire.id.clone()).unwrap();
        settle(&mut wire);
        send_ump(&wire, moved_block(6), 40);
        let changed = streams(wire.session.poll(wire.at(40)));
        let first_group = || {
            wire.session.stream(&wire.id).unwrap().capability().blocks[0]
                .first_group()
                .get()
        };
        if is_static {
            assert!(changed.is_empty(), "{changed:?}");
            assert_eq!(first_group(), 0);
        } else {
            assert_eq!(changed.len(), 1);
            assert_eq!(first_group(), 6);
        }
    }
}

/// A JR Clock or JR Timestamp a device sends (the session asks for none) is
/// dropped at the input edge: the note beside them is the only thing the graph
/// hears, and the port is not counted as having lost anything.
#[test]
fn a_jr_message_a_device_sends_never_reaches_the_graph() {
    use crate::testing::Rig;

    use tutti_ump::Utility;

    let inlets = tutti_midi_runtime::DeviceInlets::new(
        NonZeroUsize::new(2).unwrap(),
        tutti_graph::EventCapacity::of(64),
        Duration::ZERO,
    );
    let ports = Arc::new(HardwareMidiInputs::new(Arc::clone(&inlets)));
    let mut rig = Rig::over(&inlets, Instant::now());
    rig.run(1);
    let mut producer = ports.open_input_port("dev").unwrap();
    rig.apply();
    let note = midi::note_on(MidiChannel::FIRST, 60, 0x8000);
    let arrival = rig.origin + Duration::from_millis(1);
    for jr in [Utility::JrClock(10), Utility::JrTimestamp(20)] {
        assert_eq!(
            producer.push(Ump::from(Message::Utility(jr)), arrival),
            Pushed::Diverted
        );
    }
    assert_eq!(producer.push(note, arrival), Pushed::Sent);
    rig.run(8);
    let heard: Vec<_> = rig.take().into_iter().map(|(_, _, ump)| ump).collect();
    assert_eq!(heard, [note]);
    assert_eq!(ports.losses().dropped(), 0);
}

/// An output connected after its device settled is in the settled Protocol from
/// its first send, not the OS's description (here MIDI 2.0, the device MIDI 1.0).
#[test]
fn an_output_connected_after_the_settle_starts_in_the_settled_protocol() {
    let device = Endpoint::new(Capabilities {
        midi1: true,
        ..Capabilities::default()
    })
    .with_function_block(block());
    let mut wire = session_over(UmpCapability::midi2(), PortDirection::Duplex, device);
    wire.session.connect_input(&wire.id.clone()).unwrap();
    assert_eq!(settle(&mut wire).config().protocol, Protocol::Midi1);
    wire.connect_output();
    wire.send(&[(Offset::ZERO, midi::note_on(MidiChannel::FIRST, 60, 0xFFFF))]);
    assert_eq!(received(&wire)[0].message_type(), 2);
}

mod ci;

/// A device's SysEx identity is not its OS listing, which `learned` leaves out.
fn model_identity(model: u8) -> tutti_ump::DeviceIdentity {
    use tutti_ump::U7;
    tutti_ump::DeviceIdentity {
        model: [U7::of(model), U7::of(0)],
        ..identity()
    }
}

/// What a device says of itself is read back from the open input: nothing
/// before it answers (the OS's description is not its word), its Identity
/// Reply after (M1 p.40). Saved, it is compared with what answers later: a
/// Device Identity can refute a saved device but never name the unit.
#[test]
fn a_midi1_device_that_answers_an_identity_request_is_learned_and_checked_against_a_saved_one() {
    use tutti_midi_device::{Learned, Match};

    let mut wire = midi1_wire();
    assert!(wire.session.poll(wire.at(0)).next().is_none());
    assert!(
        wire.session.learned(&wire.id).is_empty(),
        "a described device has said nothing"
    );
    reply_at(&wire, identity_reply(0x05), 10);
    let _ = wire.session.poll(wire.at(20)).count();

    let learned = wire.session.learned(&wire.id);
    assert_eq!(learned, Learned::new().with_identity(identity()));
    let saved = wire.session.saved(&wire.id).expect("an open, listed input");
    assert_eq!(saved.info().id, wire.id);
    assert_eq!(saved.learned(), &learned);

    // Another model answers where this one was saved; the same model might be
    // another unit of it.
    assert_eq!(
        saved.confirm(&Learned::new().with_identity(model_identity(9))),
        Match::Different
    );
    assert_eq!(saved.confirm(&learned), Match::SameModel);

    let closed = wire.id.clone();
    wire.session.disconnect_input(&closed);
    assert!(wire.session.learned(&closed).is_empty());
    assert!(wire.session.saved(&closed).is_none());
}

/// A UMP Endpoint that answers declares its Device Identity and Product
/// Instance Id (M2-104 §7.1.3, §7.1.5); both are what `learned` reports, and an
/// equal Product Instance Id is what makes a saved device the same unit.
#[test]
fn an_endpoint_that_answers_declares_its_identity_and_product_instance_id() {
    use tutti_midi_device::{Learned, Match, ProductInstanceId};

    let device = offering_both()
        .with_identity(identity())
        .with_instance_id(ProductInstanceId::new("SN-0042").unwrap());
    let mut wire = session_over(UmpCapability::midi2(), PortDirection::Duplex, device);
    wire.session.connect_input(&wire.id.clone()).unwrap();
    assert!(wire.session.learned(&wire.id).is_empty(), "not settled yet");
    let stream = settle(&mut wire);
    assert_eq!(stream.basis(), Basis::Negotiated);

    let learned = wire.session.learned(&wire.id);
    assert_eq!(learned.identity(), Some(&identity()));
    assert_eq!(
        learned.instance_id().map(ProductInstanceId::as_str),
        Some("SN-0042")
    );
    let saved = wire.session.saved(&wire.id).unwrap();
    assert_eq!(saved.confirm(&learned), Match::SameUnit);
    let other_unit = Learned::new()
        .with_identity(identity())
        .with_instance_id(ProductInstanceId::new("SN-0043").unwrap());
    assert_eq!(saved.confirm(&other_unit), Match::Different);
}
