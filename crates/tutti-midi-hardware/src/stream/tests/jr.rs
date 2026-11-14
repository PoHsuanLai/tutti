//! JR Timestamps a device sends, against a fake device: whether the session asks
//! for them, and where the events land in the graph.

use super::*;
use crate::testing::{RATE, Rig};
use tutti_ump::Utility;

/// Sender ticks of `ms` milliseconds on a clock of 31 250 ticks a second; the
/// times used are whole ticks.
const fn ticks(ms: u64) -> u16 {
    (ms * 125 / 4) as u16
}

/// The fake device's latency: a JR Clock sent at `ms` arrives at `ms` plus this.
const LATENCY: u64 = 5;

fn jr(utility: Utility) -> Ump {
    Ump::from(Message::Utility(utility))
}

/// A connected, settled wire and a graph reading its inlets.
fn settled(timing: InputTiming, device: Endpoint) -> (Wire, Rig, DeviceStream) {
    let mut wire = session_over(UmpCapability::midi2(), PortDirection::Duplex, device);
    wire.session
        .connect_input_timed(&wire.id.clone(), timing)
        .unwrap();
    let stream = settle(&mut wire);
    let mut rig = Rig::over(wire.session.ports().inlets(), wire.start);
    rig.run(1);
    rig.apply();
    (wire, rig, stream)
}

/// The device sends JR Clocks (sent every 100 ms from 0 to 300), then a note
/// sent at 312 ms whose JR Timestamp says so, and which arrives 15 ms late:
/// `LATENCY` + 10. Returns the frame the graph hears the note on, with
/// everything else it heard.
fn note_delayed_by_10ms(wire: &Wire, rig: &mut Rig) -> (u64, Vec<Ump>) {
    for sent in [0, 100, 200, 300] {
        assert!(wire.counters.send_input(
            wire.raw,
            jr(Utility::JrClock(ticks(sent))),
            wire.at(1_000 + sent + LATENCY)
        ));
    }
    let note = midi::note_on(MidiChannel::FIRST, 60, 0xFFFF);
    let arrival = wire.at(1_000 + 312 + LATENCY + 10);
    assert!(
        wire.counters
            .send_input(wire.raw, jr(Utility::JrTimestamp(ticks(312))), arrival)
    );
    assert!(wire.counters.send_input(wire.raw, note, arrival));
    rig.run(1_000 * 44 / 64 + 400);
    let heard = rig.take();
    let frame = heard.iter().find(|h| h.2 == note).expect("the note").1;
    (frame, heard.into_iter().map(|(_, _, ump)| ump).collect())
}

/// The frame `ms` after the wire's start, as the clock map places it.
fn frame_at(ms: u64) -> u64 {
    (ms as f64 * RATE / 1_000.0).round() as u64
}

/// Within a frame or two of `expected`.
fn assert_near(frame: u64, expected: u64) {
    assert!(
        frame.abs_diff(expected) <= 2,
        "frame {frame}, expected {expected}"
    );
}

/// M2-104 §7.1.6.2: a device sends JR Timestamps when its configuration says
/// TXJR. An input opened for them asks for TXJR (and never RXJR) and the
/// device that offers it settles with it on; one opened for arrival asks for
/// neither, though the device offers both.
/// Spec: m2-104/7.1.6.2#3
#[test]
fn txjr_is_asked_for_only_by_an_input_opened_for_jr() {
    for (timing, tx_jr) in [
        (InputTiming::JitterReduction, true),
        (InputTiming::Arrival, false),
    ] {
        let mut wire = session_over(
            UmpCapability::midi2(),
            PortDirection::Duplex,
            offering_both(),
        );
        wire.session
            .connect_input_timed(&wire.id.clone(), timing)
            .unwrap();
        let mut asked = Vec::new();
        let mut settled = None;
        for ms in 0..8 {
            let (sent, done) = wire.tick(ms);
            asked.extend(sent);
            settled = settled.or(done.into_iter().next());
        }
        let requests: Vec<_> = asked
            .into_iter()
            .filter_map(|stream| match stream {
                Stream::StreamConfigurationRequest(config) => Some(config),
                _ => None,
            })
            .collect();
        assert!(!requests.is_empty(), "a request for the Protocol");
        assert!(requests.iter().all(|c| c.tx_jr == tx_jr && !c.rx_jr));
        let config = settled.expect("settled").1.config();
        assert_eq!((config.tx_jr, config.rx_jr), (tx_jr, false));
    }
}

/// Once TXJR is on, an event is placed at the time its JR Timestamp maps to,
/// not at its arrival: the note arrived 10 ms after the connection's usual
/// latency and is heard where it was sent (plus the latency, the least a
/// receiver can know of). The JR messages are not heard.
/// Spec: m2-104/7.2.2.2#1
#[test]
fn a_jr_input_places_an_event_at_its_timestamp_not_its_arrival() {
    let (wire, mut rig, stream) = settled(InputTiming::JitterReduction, offering_both());
    assert!(stream.config().tx_jr);
    let (frame, heard) = note_delayed_by_10ms(&wire, &mut rig);
    assert_near(frame, frame_at(1_000 + 312 + LATENCY));
    assert_eq!(heard.len(), 1, "the JR messages never reach the graph");
}

/// With TXJR off (the input was opened for arrival), the same traffic leaves the
/// event at its arrival stamp, and the JR messages are still not heard. Also
/// where the device turns TXJR on by itself: the input did not ask for it.
#[test]
fn an_arrival_input_keeps_the_arrival_stamp_though_the_device_sends_jr() {
    let (mut wire, mut rig, stream) = settled(InputTiming::Arrival, offering_both());
    assert!(!stream.config().tx_jr);
    let on = Stream::StreamConfigurationNotification(StreamConfig {
        protocol: Protocol::Midi2,
        rx_jr: false,
        tx_jr: true,
    });
    send_ump(&wire, on, 20);
    assert_eq!(streams(wire.poll(30)).len(), 1);
    assert!(wire.session.stream(&wire.id).unwrap().config().tx_jr);
    let (frame, heard) = note_delayed_by_10ms(&wire, &mut rig);
    assert_near(frame, frame_at(1_000 + 312 + LATENCY + 10));
    assert_eq!(heard.len(), 1);
}

/// A device that does not offer TXJR settles with it off, so an input opened for
/// JR is placed by arrival as any other.
#[test]
fn a_jr_input_of_a_device_without_txjr_is_timed_by_arrival() {
    let device = Endpoint::new(Capabilities {
        midi1: true,
        midi2: true,
        rx_jr: true,
        tx_jr: false,
    })
    .with_function_block(block());
    let (wire, mut rig, stream) = settled(InputTiming::JitterReduction, device);
    assert!(!stream.config().tx_jr);
    let (frame, heard) = note_delayed_by_10ms(&wire, &mut rig);
    assert_near(frame, frame_at(1_000 + 312 + LATENCY + 10));
    assert_eq!(heard.len(), 1);
}

/// M2-104 §7.2.2.3: a message with a JR Timestamp before any JR Clock is
/// rendered as soon as possible: the event is placed at its arrival, and the
/// timestamp is not heard.
/// Spec: m2-104/7.2.2.3#1
#[test]
fn a_timestamp_before_any_clock_places_the_event_at_its_arrival() {
    let (wire, mut rig, stream) = settled(InputTiming::JitterReduction, offering_both());
    assert!(stream.config().tx_jr);
    let note = midi::note_on(MidiChannel::FIRST, 60, 0xFFFF);
    let arrival = wire.at(1_000 + 50);
    assert!(
        wire.counters
            .send_input(wire.raw, jr(Utility::JrTimestamp(ticks(10))), arrival)
    );
    assert!(wire.counters.send_input(wire.raw, note, arrival));
    rig.run(1_000 * 44 / 64 + 100);
    let heard = rig.take();
    assert_eq!(heard.len(), 1);
    assert_near(heard[0].1, frame_at(1_000 + 50));
}

/// Events keep the order they arrived in (M2-104 §7.2.2.2): a second note whose
/// timestamp maps earlier than the first's is not placed before it.
/// Spec: m2-104/7.2.2.2#3
#[test]
fn events_are_placed_in_the_order_they_arrive() {
    let (wire, mut rig, _) = settled(InputTiming::JitterReduction, offering_both());
    for sent in [0, 100, 200, 300] {
        assert!(wire.counters.send_input(
            wire.raw,
            jr(Utility::JrClock(ticks(sent))),
            wire.at(1_000 + sent + LATENCY)
        ));
    }
    let first = midi::note_on(MidiChannel::FIRST, 60, 0xFFFF);
    let second = midi::note_on(MidiChannel::FIRST, 62, 0xFFFF);
    let arrival = wire.at(1_000 + 330);
    for (ms, note) in [(320, first), (310, second)] {
        assert!(
            wire.counters
                .send_input(wire.raw, jr(Utility::JrTimestamp(ticks(ms))), arrival)
        );
        assert!(wire.counters.send_input(wire.raw, note, arrival));
    }
    rig.run(1_000 * 44 / 64 + 400);
    let heard = rig.take();
    let at = |ump| heard.iter().find(|h| h.2 == ump).expect("heard").1;
    assert_eq!(heard.len(), 2);
    assert!(at(first) <= at(second));
}
