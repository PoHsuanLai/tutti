//! Virtual endpoints against the kernel's sequencer: another client subscribes
//! to what this process publishes, as another application does.
//!
//! Each test names its endpoints with its own process id, so tests (each its
//! own process under nextest) and other clients on the machine do not meet.

use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

use super::listener::Listener;

use tutti_alsa_ffi::{
    Blocking, CAP_READ, CAP_WRITE, Client, PROTO_MIDI1, PROTO_MIDI2, PortEntry, Read,
    TYPE_APPLICATION, TYPE_MIDI_GENERIC, UmpVersion,
};
use tutti_fixtures::midi::{m1_note_on, note_on, per_note_pitch_bend};
use tutti_graph::EventCapacity;
use tutti_midi_device::{BlockName, EndpointName, FunctionBlock};
use tutti_midi_hardware::{
    DeviceInlets, HardwareMidiInputs, VirtualDestination, VirtualProtocol, VirtualSource,
    VirtualSpec,
};
use tutti_ump::{
    Direction, FirstGroup, FunctionBlockId, GroupSpan, Message, MidiChannel, U7, UiHint, Ump,
    Utility,
};

const PATIENCE: Duration = Duration::from_secs(5);

fn spec(tag: &str) -> VirtualSpec {
    VirtualSpec::new(
        EndpointName::new(&format!("tutti-virt-{}-{tag}", std::process::id())).unwrap(),
    )
}

/// The sequencer's port of the endpoint `spec` published.
fn find(peer: &Client, spec: &VirtualSpec) -> Option<PortEntry> {
    peer.list_ports()
        .ports
        .into_iter()
        .find(|p| p.client_name == spec.name().as_str())
}

fn peer(caps: u32) -> (Client, i32) {
    peer_in(caps, UmpVersion::Midi2)
}

/// A peer client delivered `version`.
fn peer_in(caps: u32, version: UmpVersion) -> (Client, i32) {
    let client = Client::open("tutti-virt-peer", Blocking::NonBlock, version).expect("a sequencer");
    let port = client
        .open_port("peer", caps, TYPE_MIDI_GENERIC | TYPE_APPLICATION)
        .expect("a port");
    (client, port)
}

fn notes(count: u8) -> Vec<Ump> {
    (0..count)
        .map(|n| note_on(MidiChannel::of(2), 60 + n, 0x8000 + u16::from(n)))
        .collect()
}

/// Reads `count` UMP messages the peer receives, in arrival order.
fn read(peer: &Client, count: usize) -> Vec<[u32; 4]> {
    let deadline = Instant::now() + PATIENCE;
    let mut got = Vec::new();
    while got.len() < count {
        assert!(Instant::now() < deadline, "only {got:?} arrived");
        match peer.read_event().expect("a read") {
            Read::Event(event) => got.push(event.ump),
            Read::Empty | Read::Skipped => std::thread::sleep(Duration::from_millis(1)),
        }
    }
    got
}

/// What a subscriber of a virtual source receives is what was sent into it, each
/// message whole and in the order sent.
#[test]
fn a_subscriber_receives_what_a_virtual_source_sends_in_order() {
    let spec = spec("source");
    let source = VirtualSource::open(&spec).expect("a virtual source");
    let (peer, port) = peer(CAP_WRITE);
    let found = find(&peer, &spec).expect("the source is listed");
    peer.connect_from(port, found.client, found.port)
        .expect("subscribes");

    let sent = notes(5);
    for ump in &sent {
        source.send(*ump).expect("sends");
    }
    let got = read(&peer, sent.len());
    for (ump, got) in sent.iter().zip(&got) {
        assert_eq!(&got[..ump.words().len()], ump.words());
    }
}

/// What a client writes to a virtual destination reaches the graph, each message
/// whole and in the order sent.
#[test]
fn a_virtual_destination_delivers_what_a_client_sends_to_the_graph_in_order() {
    let spec = spec("destination");
    let inlets = DeviceInlets::new(NonZeroUsize::MIN, EventCapacity::of(64), Duration::ZERO);
    let mut listener = Listener::over(&inlets);
    let ports = HardwareMidiInputs::new(std::sync::Arc::clone(&inlets));
    let producer = ports.open_input_port("virtual").expect("a slot");
    listener.apply();
    let destination = VirtualDestination::open(&spec, producer).expect("a virtual destination");
    let (peer, port) = peer(CAP_READ);
    let found = find(&peer, &spec).expect("the destination is listed");
    peer.connect_to(port, found.client, found.port)
        .expect("subscribes");

    let sent = notes(5);
    for ump in &sent {
        peer.send_to_subscribers(u8::try_from(port).unwrap(), ump.words())
            .expect("sends");
    }
    let heard = listener.listen(Duration::from_millis(300));
    assert_eq!(heard, sent);
    assert!(destination.is_alive());
}

/// JR Clock and JR Timestamp messages (M2-104 §7.2.2) a client sends through the
/// kernel's UMP sequencer are not heard by a port timed by arrival, which hears
/// the note beside them.
#[test]
fn jr_messages_a_client_sends_are_not_heard_by_an_arrival_port() {
    let spec = spec("jr");
    let inlets = DeviceInlets::new(NonZeroUsize::MIN, EventCapacity::of(64), Duration::ZERO);
    let mut listener = Listener::over(&inlets);
    let ports = HardwareMidiInputs::new(std::sync::Arc::clone(&inlets));
    let producer = ports.open_input_port("virtual").expect("a slot");
    listener.apply();
    let _destination = VirtualDestination::open(&spec, producer).expect("a virtual destination");
    let (peer, port) = peer(CAP_READ);
    let found = find(&peer, &spec).expect("the destination is listed");
    peer.connect_to(port, found.client, found.port)
        .expect("subscribes");

    let clock = Ump::from(Message::Utility(Utility::JrClock(0x1234)));
    let stamp = Ump::from(Message::Utility(Utility::JrTimestamp(0x1300)));
    let note = note_on(MidiChannel::of(2), 60, 0x8000);
    for ump in [clock, stamp, note] {
        peer.send_to_subscribers(u8::try_from(port).unwrap(), ump.words())
            .expect("sends");
    }
    assert_eq!(listener.listen(Duration::from_millis(300)), [note]);
    assert_eq!(ports.losses().dropped(), 0);
}

/// A block of Groups 4 to 5, named, bidirectional, a receiver's hint.
fn block() -> FunctionBlock {
    FunctionBlock::builder(
        FunctionBlockId::new(1).unwrap(),
        Direction::Bidirectional,
        FirstGroup::new(4).unwrap(),
        GroupSpan::new(2).unwrap(),
    )
    .ui_hint(UiHint::Receiver)
    .midi_ci(2, 3)
    .name(BlockName::new("Pads").unwrap())
    .build()
    .unwrap()
}

/// The endpoint record another client reads for the endpoint carries the
/// protocol it was created with and the Function Blocks of its spec (M2-104
/// §7.1.2 and §7.1.8), for a source and a destination alike.
#[test]
fn the_published_endpoint_record_carries_the_protocol_and_the_blocks() {
    let source_spec = spec("record-source").with_function_block(block());
    let dest_spec = spec("record-destination").with_function_block(block());
    let source = VirtualSource::open(&source_spec).expect("a virtual source");
    let inlets = DeviceInlets::new(NonZeroUsize::MIN, EventCapacity::of(8), Duration::ZERO);
    let ports = HardwareMidiInputs::new(inlets);
    let producer = ports.open_input_port("virtual").expect("a slot");
    let destination = VirtualDestination::open(&dest_spec, producer).expect("a destination");
    assert_eq!(source.protocol(), VirtualProtocol::Midi2);
    assert_eq!(destination.protocol(), VirtualProtocol::Midi2);

    let (peer, _) = peer(CAP_READ);
    for spec in [&source_spec, &dest_spec] {
        let record = find(&peer, spec)
            .expect("listed")
            .ump
            .expect("a UMP Endpoint record");
        assert_eq!(record.protocol, PROTO_MIDI2);
        assert_eq!(record.protocol_caps, PROTO_MIDI2);
        let [got] = record.blocks.as_slice() else {
            panic!("one block, got {:?}", record.blocks);
        };
        assert_eq!(got.id, 1);
        assert!(got.active);
        assert_eq!(got.direction, tutti_alsa_ffi::DIR_BIDIRECTION);
        assert_eq!((got.first_group, got.num_groups), (4, 2));
        assert_eq!(got.ui_hint, 1);
        assert_eq!((got.midi_ci_version, got.sysex8_streams), (2, 3));
        assert_eq!(got.name.as_deref(), Some("Pads"));
    }
}

/// An endpoint created as MIDI 1.0 reports MIDI 1.0 and publishes it, and the
/// MIDI 1.0 channel voice message (type 2) a client sends it reaches the graph's
/// ingest node as it does from a MIDI 1.0 device, which widens it: velocity
/// `0x40` is `0x8000` (M2-115).
#[test]
fn a_midi1_destination_reports_midi1_and_its_messages_arrive_widened() {
    let spec = spec("midi1").with_protocol(VirtualProtocol::Midi1);
    let inlets = DeviceInlets::new(NonZeroUsize::MIN, EventCapacity::of(8), Duration::ZERO);
    let mut listener = Listener::over_ingest(&inlets);
    let ports = HardwareMidiInputs::new(std::sync::Arc::clone(&inlets));
    let producer = ports.open_input_port("virtual").expect("a slot");
    listener.apply();
    let destination = VirtualDestination::open(&spec, producer).expect("a destination");
    assert_eq!(destination.protocol(), VirtualProtocol::Midi1);

    let (peer, port) = peer(CAP_READ);
    let found = find(&peer, &spec).expect("listed");
    assert_eq!(found.ump.as_ref().map(|e| e.protocol), Some(PROTO_MIDI1));
    peer.connect_to(port, found.client, found.port)
        .expect("subscribes");
    let note = m1_note_on(MidiChannel::of(2), 60, U7::of(0x40));
    peer.send_to_subscribers(u8::try_from(port).unwrap(), note.words())
        .expect("sends");
    let heard = listener.listen(Duration::from_millis(300));
    assert_eq!(heard, [note_on(MidiChannel::of(2), 60, 0x8000)]);
}

/// A MIDI 2.0 note sent into a MIDI 1.0 source reaches a subscriber as the
/// MIDI 1.0 note (type 2): the 16-bit velocity `0x8100` is the 7-bit `0x40`
/// (M2-115).
#[test]
fn a_midi2_note_sent_into_a_midi1_source_arrives_as_a_midi1_note() {
    let spec = spec("narrow").with_protocol(VirtualProtocol::Midi1);
    let source = VirtualSource::open(&spec).expect("a virtual source");
    assert_eq!(source.protocol(), VirtualProtocol::Midi1);
    let (peer, port) = peer_in(CAP_WRITE, UmpVersion::Midi1);
    let found = find(&peer, &spec).expect("listed");
    peer.connect_from(port, found.client, found.port)
        .expect("subscribes");

    source
        .send(note_on(MidiChannel::of(2), 60, 0x8100))
        .expect("sends");
    let got = read(&peer, 1);
    let want = m1_note_on(MidiChannel::of(2), 60, U7::of(0x40));
    assert_eq!(&got[0][..want.words().len()], want.words());
}

/// A MIDI 2.0 message with no MIDI 1.0 form (M2-104 Appendix D.2: a per-note
/// pitch bend) is refused by name by a MIDI 1.0 source, and nothing reaches the
/// subscriber.
#[test]
fn a_message_with_no_midi1_form_is_refused_by_a_midi1_source() {
    let spec = spec("noform").with_protocol(VirtualProtocol::Midi1);
    let source = VirtualSource::open(&spec).expect("a virtual source");
    let (peer, port) = peer_in(CAP_WRITE, UmpVersion::Midi1);
    let found = find(&peer, &spec).expect("listed");
    peer.connect_from(port, found.client, found.port)
        .expect("subscribes");

    let bend = per_note_pitch_bend(MidiChannel::of(2), 60, 0x8000_0000);
    assert!(matches!(
        source.send(bend),
        Err(tutti_midi_hardware::Error::NoMidi1Form(
            tutti_midi_protocol::NoMidi1Form::PerNote
        ))
    ));
    std::thread::sleep(Duration::from_millis(100));
    assert!(matches!(peer.read_event(), Ok(Read::Empty)));
}

/// A MIDI 1.0 note sent into a MIDI 2.0 source reaches a subscriber widened:
/// velocity `0x40` is `0x8000` (M2-115).
#[test]
fn a_midi1_note_sent_into_a_midi2_source_arrives_widened() {
    let spec = spec("widen");
    let source = VirtualSource::open(&spec).expect("a virtual source");
    let (peer, port) = peer(CAP_WRITE);
    let found = find(&peer, &spec).expect("listed");
    peer.connect_from(port, found.client, found.port)
        .expect("subscribes");

    source
        .send(m1_note_on(MidiChannel::of(2), 60, U7::of(0x40)))
        .expect("sends");
    let got = read(&peer, 1);
    let want = note_on(MidiChannel::of(2), 60, 0x8000);
    assert_eq!(&got[0][..want.words().len()], want.words());
}
