use super::super::CoreMidiEndpoints;
use super::*;
use crate::endpoints::MidiEndpoints;
use std::sync::mpsc;
use std::time::Duration;
use tutti_fixtures::midi;
use tutti_graph::Offset;
use tutti_ump::{DiscoveryFilter, Message, MidiChannel, Protocol, Stream, Ump, UmpVersion};

/// **A packet sent to the destination reaches its callback, every message of
/// it, in order** (M2-104 §7.1: a UMP Stream message has no MIDI 1.0 form, so
/// this endpoint is the only way one arrives). An output port of this process
/// sends an Endpoint Discovery and a MIDI 2.0 note to the destination, as
/// another app would; CoreMIDI delivers them on its own thread with a null
/// connection reference, which the receive block must not read.
#[test]
fn a_packet_sent_to_the_destination_reaches_its_callback() {
    let (tx, rx) = mpsc::channel();
    let name = "tutti-vdest-delivery";
    let _destination = UmpVirtualDestination::new(name, move |ump| {
        let _ = tx.send(ump);
    })
    .expect("a virtual UMP destination");
    let backend = CoreMidiEndpoints::new();
    let id = backend
        .outputs()
        .into_iter()
        .find(|e| e.name.contains(name))
        .expect("the virtual destination is listed")
        .id;
    let out = backend.open_output(id).expect("opens");
    out.follow(Protocol::Midi2);

    let discovery = Ump::from(Message::Stream(Stream::EndpointDiscovery {
        version: UmpVersion { major: 1, minor: 1 },
        filter: DiscoveryFilter {
            endpoint_info: true,
            device_identity: true,
            endpoint_name: true,
            product_instance_id: true,
            stream_configuration: true,
        },
    }));
    let note = midi::note_on(MidiChannel::of(3), 60, 0x8000);
    assert_eq!(
        out.queue(&[(Offset::ZERO, discovery), (Offset::ZERO, note)]),
        2
    );

    let wait = Duration::from_secs(2);
    let got = [
        rx.recv_timeout(wait).expect("the discovery arrives"),
        rx.recv_timeout(wait).expect("the note arrives"),
    ];
    assert_eq!(got, [discovery, note]);
}
