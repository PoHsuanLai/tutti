//! The UMP Stream negotiation against a fake device that answers as
//! M2-104 §7.1 says an Endpoint does (`Endpoint::reply`), through the session
//! and the fake backend: the wire is the fake's sink and the input's
//! producer.

use super::*;
use crate::HardwareMidiInputs;
use crate::MidiSession;
use crate::test_support::{FakeBackend, FakeCounters, endpoint};
use std::sync::Arc;
use std::time::Duration;
use tutti_midi_device::{
    BlockName, Capabilities, DeviceId, Direction, EndpointId, FunctionBlock, MAX_ATTEMPTS,
    STEP_TIMEOUT,
};
use tutti_ump::{
    Direction as BlockDirection, FirstGroup, FunctionBlockId, GroupSpan, Message, Stream,
};

/// A fake duplex device `raw` and the session that opened its input.
struct Wire {
    session: MidiSession,
    counters: FakeCounters,
    id: DeviceId,
    raw: EndpointId,
    /// The device's own state, which answers what it is sent.
    device: Endpoint,
    start: Instant,
}

fn session_over(os: UmpCapability, direction: Direction, device: Endpoint) -> Wire {
    let (backend, counters) = FakeBackend::build_with(vec![], vec![], usize::MAX);
    let ports = Arc::new(HardwareMidiInputs::new(Arc::new(
        tutti_midi_runtime::DeviceInlets::new(4, 64, Duration::ZERO),
    )));
    let session = MidiSession::open_over(backend, ports).unwrap();
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

    /// One tick of the session at `ms`, then the device reads what the session
    /// sent and answers into the input. Returns what the session sent, and the
    /// devices the tick settled.
    fn tick(&mut self, ms: u64) -> (Vec<Stream>, Vec<(DeviceId, DeviceStream)>) {
        let now = self.at(ms);
        let settled = self.session.poll(now);
        let sent: Vec<Stream> = self
            .counters
            .take_sent(self.raw)
            .into_iter()
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
    FunctionBlock::new(
        FunctionBlockId::new(0).unwrap(),
        BlockDirection::Bidirectional,
        FirstGroup::new(0).unwrap(),
        GroupSpan::new(2).unwrap(),
    )
    .unwrap()
    .with_name(BlockName::new("Main").unwrap())
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
    let wire = session_over(UmpCapability::midi1(), Direction::Duplex, offering_both());
    // The OS says MIDI 1.0 with no blocks: not a UMP Endpoint, nothing asked.
    wire.session.connect_input(&wire.id.clone()).unwrap();
    assert_eq!(
        wire.session.stream(&wire.id).unwrap().basis(),
        Basis::Described
    );

    let mut wire = session_over(UmpCapability::midi2(), Direction::Duplex, offering_both());
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
    let mut wire = session_over(UmpCapability::midi2(), Direction::Duplex, device);
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
    let wire = session_over(UmpCapability::midi2(), Direction::Duplex, silent);
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
        settled.extend(wire.session.poll(now));
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
/// packet is sent, and the device is `Described` from the connect (no wait).
#[test]
fn a_midi1_port_is_not_asked_and_is_described_at_once() {
    let mut wire = session_over(
        UmpCapability::midi1(),
        Direction::Duplex,
        Endpoint::new(Capabilities::default()),
    );
    wire.session.connect_input(&wire.id.clone()).unwrap();
    let stream = wire.session.stream(&wire.id).expect("settled at connect");
    assert_eq!(stream.basis(), Basis::Described);
    assert_eq!(stream.config().protocol, Protocol::Midi1);
    let (sent, settled) = wire.tick(0);
    assert!(sent.is_empty() && settled.is_empty());
}

/// A UMP Endpoint the session can only read (no output) cannot be asked:
/// `Described`, as the OS says, with no wait.
#[test]
fn an_endpoint_with_no_output_is_described_not_waited_for() {
    let wire = session_over(
        UmpCapability::midi2(),
        Direction::Input,
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
    use tutti_fixtures::midi::note_on;
    use tutti_ump::{MidiChannel, MidiGroup};

    let inlets = Arc::new(tutti_midi_runtime::DeviceInlets::new(2, 64, Duration::ZERO));
    let ports = Arc::new(HardwareMidiInputs::new(Arc::clone(&inlets)));
    let start = Instant::now();
    let mut rig = Rig::over(&inlets, start);
    rig.run(1);
    let mut producer = ports.open_input_port("dev").unwrap();
    let port = producer.port_id();
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
    let note = note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000);
    let arrival = rig.origin + Duration::from_millis(1);
    assert!(producer.push(stream, arrival));
    assert!(producer.push(note, arrival));
    rig.run(8);
    let heard: Vec<_> = rig.take().into_iter().map(|(_, _, ump)| ump).collect();
    assert_eq!(heard, [note]);
    let mut said = Vec::new();
    ports.drain_stream(port, |ump| said.push(ump));
    assert_eq!(said, [stream]);
}

// --- The output follows what the device said -------------------------------

/// What the fake device received on its output, as the packets.
fn received(wire: &Wire) -> Vec<Ump> {
    wire.counters.take_sent(wire.raw)
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
            value,
        },
    ))
}

/// M2-104 §3.2: a MIDI 1.0 endpoint "shall not use any messages from Message
/// Type 0x4". A device the OS describes as a MIDI 1.0 port (no UMP Stream is
/// asked of it) is sent only type 2 whatever the graph carries, and a
/// Registered Controller arrives as the Control Changes 101, 100, 6 and 38 of
/// Appendix D.2.3.
#[test]
fn a_midi1_port_is_never_sent_type_4() {
    use tutti_fixtures::midi::note_on;
    use tutti_ump::{MidiChannel, MidiGroup};
    let wire = session_over(UmpCapability::midi1(), Direction::Output, offering_both());
    wire.session.connect_output(&wire.id).unwrap();
    let note = note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xFFFF);
    assert_eq!(
        wire.session
            .send(&[(Offset::ZERO, note), (Offset::ZERO, rpn_ump(12 << 25))]),
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
    use tutti_fixtures::midi::note_on;
    use tutti_ump::{MidiChannel, MidiGroup};
    let device = Endpoint::new(Capabilities {
        midi1: true,
        midi2: true,
        ..Capabilities::default()
    })
    .with_function_block(block());
    // A device that has MIDI 2.0 on offer but is in MIDI 1.0; the session asks
    // for MIDI 2.0, so it ends in MIDI 2.0 ...
    let mut wire = session_over(UmpCapability::midi2(), Direction::Duplex, device);
    wire.session.connect_input(&wire.id.clone()).unwrap();
    wire.session.connect_output(&wire.id).unwrap();
    let note = [(
        Offset::ZERO,
        note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xFFFF),
    )];
    // ... but until then the output is what the OS said: MIDI 2.0.
    wire.session.send(&note);
    assert_eq!(received(&wire)[0].message_type(), 4);

    let stream = settle(&mut wire);
    assert_eq!(stream.config().protocol, Protocol::Midi2);
    received(&wire);
    wire.session.send(&note);
    assert_eq!(received(&wire)[0].message_type(), 4);

    // The device goes to MIDI 1.0 on its own: it is told, and the output goes
    // with it.
    send_ump(&wire, notification(Protocol::Midi1, false), 20);
    let changed = wire.session.poll(wire.at(20));
    assert!(
        matches!(changed.as_slice(), [(_, s)] if s.config().protocol == Protocol::Midi1),
        "{changed:?}"
    );
    wire.session.send(&note);
    assert_eq!(received(&wire)[0].message_type(), 2);
    assert_eq!(
        wire.session.output_config().map(|c| c.protocol),
        Some(Protocol::Midi1)
    );
}

/// M2-104 §7.1.6.2: JR Timestamps are sent "until after a Stream Configuration
/// Notification ... has been received". The session asks for none, so a JR
/// Clock the output is handed does not reach the device; after the device
/// notifies that it receives them, it does.
#[test]
fn jr_reaches_a_device_only_after_its_notification_enables_it() {
    let mut wire = session_over(UmpCapability::midi2(), Direction::Duplex, offering_both());
    wire.session.connect_input(&wire.id.clone()).unwrap();
    let stream = settle(&mut wire);
    assert!(!stream.config().rx_jr);
    wire.session.connect_output(&wire.id).unwrap();
    let clock = Ump::from(Message::Utility(tutti_ump::Utility::JrClock(7)));
    let note = tutti_fixtures::midi::note_on(
        tutti_ump::MidiGroup::FIRST,
        tutti_ump::MidiChannel::FIRST,
        60,
        0xFFFF,
    );
    let events = [(Offset::ZERO, clock), (Offset::ZERO, note)];
    received(&wire);
    assert_eq!(wire.session.send(&events), 2, "nothing was refused");
    assert_eq!(received(&wire), [note]);

    send_ump(&wire, notification(Protocol::Midi2, true), 30);
    assert_eq!(wire.session.poll(wire.at(30)).len(), 1);
    wire.session.send(&events);
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
        let mut wire = session_over(UmpCapability::midi2(), Direction::Duplex, device);
        wire.session.connect_input(&wire.id.clone()).unwrap();
        settle(&mut wire);
        send_ump(&wire, moved_block(6), 40);
        let changed = wire.session.poll(wire.at(40));
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
    use tutti_fixtures::midi::note_on;
    use tutti_ump::{MidiChannel, MidiGroup, Utility};

    let inlets = Arc::new(tutti_midi_runtime::DeviceInlets::new(2, 64, Duration::ZERO));
    let ports = Arc::new(HardwareMidiInputs::new(Arc::clone(&inlets)));
    let mut rig = Rig::over(&inlets, Instant::now());
    rig.run(1);
    let mut producer = ports.open_input_port("dev").unwrap();
    rig.apply();
    let note = note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000);
    let arrival = rig.origin + Duration::from_millis(1);
    for jr in [Utility::JrClock(10), Utility::JrTimestamp(20)] {
        assert!(producer.push(Ump::from(Message::Utility(jr)), arrival));
    }
    assert!(producer.push(note, arrival));
    rig.run(8);
    let heard: Vec<_> = rig.take().into_iter().map(|(_, _, ump)| ump).collect();
    assert_eq!(heard, [note]);
    assert_eq!(ports.dropped_events(), 0);
}

/// An output connected after its device settled is in the settled Protocol from
/// its first send, not the OS's description (here MIDI 2.0, the device MIDI 1.0).
#[test]
fn an_output_connected_after_the_settle_starts_in_the_settled_protocol() {
    use tutti_fixtures::midi::note_on;
    use tutti_ump::{MidiChannel, MidiGroup};
    let device = Endpoint::new(Capabilities {
        midi1: true,
        ..Capabilities::default()
    })
    .with_function_block(block());
    let mut wire = session_over(UmpCapability::midi2(), Direction::Duplex, device);
    wire.session.connect_input(&wire.id.clone()).unwrap();
    assert_eq!(settle(&mut wire).config().protocol, Protocol::Midi1);
    wire.session.connect_output(&wire.id).unwrap();
    wire.session.send(&[(
        Offset::ZERO,
        note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xFFFF),
    )]);
    assert_eq!(received(&wire)[0].message_type(), 2);
}
