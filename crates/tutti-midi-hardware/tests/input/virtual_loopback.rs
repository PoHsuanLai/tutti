//! A virtual endpoint seen from a session of this process, on every platform
//! with a backend: the session lists the endpoint as it lists a device, and what
//! crosses it arrives whole and in order.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::listener::Listener;

use tutti_fixtures::midi::note_on;
use tutti_graph::{EventCapacity, Offset};
use tutti_midi_device::{DeviceInfo, EndpointName};
use tutti_midi_hardware::{
    DeviceInlets, MidiSession, VirtualDestination, VirtualProtocol, VirtualSource, VirtualSpec,
};
use tutti_ump::{MidiChannel, Ump};

const PATIENCE: Duration = Duration::from_secs(10);

fn spec(tag: &str) -> VirtualSpec {
    let name = format!("tutti-loop-{}-{tag}", std::process::id());
    VirtualSpec::new(EndpointName::new(&name).unwrap())
}

/// A session over four inlet slots, and the graph that plays them.
fn session() -> (MidiSession, Listener) {
    let inlets = DeviceInlets::new(
        NonZeroUsize::new(4).unwrap(),
        EventCapacity::of(64),
        Duration::ZERO,
    );
    let listener = Listener::over(&inlets);
    (MidiSession::open(inlets).expect("a session"), listener)
}

/// The device `spec` published, once the session lists it.
fn listed(find: impl Fn() -> Vec<DeviceInfo>, spec: &VirtualSpec) -> DeviceInfo {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Some(device) = find()
            .into_iter()
            .find(|d| d.name.contains(spec.name().as_str()))
        {
            return device;
        }
        assert!(
            Instant::now() < deadline,
            "{} is never listed",
            spec.name().as_str()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn notes() -> Vec<Ump> {
    (0..5)
        .map(|n| note_on(MidiChannel::of(1), 60 + n, 0x8000 + u16::from(n)))
        .collect()
}

/// What a virtual source is sent arrives at a session input connected to it, in
/// order.
#[test]
fn what_a_virtual_source_sends_reaches_a_session_input_in_order() {
    let spec = spec("source");
    let source = VirtualSource::open(&spec).expect("a virtual source");
    let (mut io, mut graph) = session();
    let device = listed(|| io.inputs(), &spec);
    io.connect_input(&device.id, tutti_midi_hardware::InputTiming::Arrival)
        .expect("connects");
    graph.apply();
    std::thread::sleep(Duration::from_millis(200));
    graph.listen(Duration::from_millis(50));

    let sent = notes();
    for ump in &sent {
        source.send(*ump).expect("sends");
    }
    assert_eq!(graph.listen(Duration::from_millis(300)), sent);
}

/// What a session output sends to a virtual destination arrives at the
/// destination's producer, in order, as the events of a device input.
#[test]
fn what_a_session_output_sends_reaches_a_virtual_destination_in_order() {
    let spec = spec("destination");
    let (io, mut graph) = session();
    let producer = Arc::clone(io.ports())
        .open_input_port("virtual")
        .expect("a slot");
    graph.apply();
    let destination = VirtualDestination::open(&spec, producer).expect("a virtual destination");
    let device = listed(|| io.outputs(), &spec);
    let mut out = io.connect_output(&device.id).expect("connects");
    std::thread::sleep(Duration::from_millis(200));
    graph.listen(Duration::from_millis(50));

    let sent = notes();
    for ump in &sent {
        assert_eq!(out.queue(&[(Offset::ZERO, *ump)]), 1);
    }
    assert_eq!(graph.listen(Duration::from_millis(300)), sent);
    assert!(destination.is_alive());
}

/// A MIDI 1.0 destination receives MIDI 1.0 (the session's output narrows for
/// it) and the graph's ingest node widens it as it does a MIDI 1.0 device's:
/// the 16-bit velocity `0x8100` is the 7-bit `0x40` on the wire and `0x8000`
/// after widening (M2-115).
#[test]
fn a_midi1_destination_delivers_what_the_ingest_node_widens() {
    let spec = spec("midi1").with_protocol(VirtualProtocol::Midi1);
    let inlets = DeviceInlets::new(
        NonZeroUsize::new(4).unwrap(),
        EventCapacity::of(64),
        Duration::ZERO,
    );
    let mut graph = Listener::over_ingest(&inlets);
    let io = MidiSession::open(inlets).expect("a session");
    let producer = Arc::clone(io.ports())
        .open_input_port("virtual")
        .expect("a slot");
    graph.apply();
    let _destination = VirtualDestination::open(&spec, producer).expect("a virtual destination");
    let device = listed(|| io.outputs(), &spec);
    let mut out = io.connect_output(&device.id).expect("connects");
    std::thread::sleep(Duration::from_millis(200));
    graph.listen(Duration::from_millis(50));

    assert_eq!(
        out.queue(&[(Offset::ZERO, note_on(MidiChannel::of(1), 60, 0x8100))]),
        1
    );
    assert_eq!(
        graph.listen(Duration::from_millis(300)),
        [note_on(MidiChannel::of(1), 60, 0x8000)]
    );
}
