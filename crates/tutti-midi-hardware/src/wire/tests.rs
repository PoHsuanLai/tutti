//! The wire form against M2-104: a MIDI 1.0 endpoint is never sent type `0x4`,
//! a Registered Controller narrows to the four Control Changes of Appendix D.2.3
//! through one narrower, and JR goes only where it is enabled.

use super::*;
use tutti_fixtures::midi;
use tutti_graph::Offset;
use tutti_ump::{
    Address, MidiChannel, MidiGroup, NoteNumber, Sysex7Packet, SysexStatus, System, U7, Voice1,
    Voice2,
};

const MIDI1: StreamConfig = StreamConfig {
    protocol: Protocol::Midi1,
    rx_jr: false,
    tx_jr: false,
};
const MIDI2: StreamConfig = StreamConfig {
    protocol: Protocol::Midi2,
    rx_jr: false,
    tx_jr: false,
};

fn at(ump: Ump) -> UmpAt {
    (Offset::ZERO, ump)
}

fn address(group: u8, channel: u8) -> Address {
    Address {
        group: MidiGroup::new(group).unwrap(),
        channel: MidiChannel::new(channel).unwrap(),
    }
}

/// A Registered Controller, bank and index, in group 0, channel 0.
fn rpn(bank: u8, index: u8, value: u32) -> Ump {
    Ump::from(Message::Midi2(
        address(0, 0),
        Voice2::Registered {
            bank: U7::of(bank),
            index: U7::of(index),
            value,
        },
    ))
}

fn convert(wire: &mut Wire, events: &[Ump]) -> Vec<Ump> {
    let events: Vec<UmpAt> = events.iter().copied().map(at).collect();
    wire.convert(&events);
    wire.batch().iter().map(|&(_, ump)| ump).collect()
}

/// The `(controller, value)` of each Control Change of a MIDI 1.0 packet list,
/// each checked to be type `0x2`.
fn controls(packets: &[Ump]) -> Vec<(u8, u8)> {
    packets
        .iter()
        .map(|&ump| {
            assert_eq!(ump.message_type(), 2, "{ump:?}");
            let Ok(Message::Midi1(_, Voice1::ControlChange { controller, value })) = decode(ump)
            else {
                panic!("not a Control Change: {ump:?}")
            };
            (controller.get(), value.get())
        })
        .collect()
}

/// M2-104 §3.2: "UMP MIDI 1.0 Devices shall not use any messages from Message
/// Type 0x4". Whatever MIDI 2.0 messages are sent to a MIDI 1.0 endpoint, no
/// packet of the batch is type 4, and the ones with a MIDI 1.0 form come out as
/// type 2 on the same Group and Channel.
#[test]
fn a_midi1_endpoint_is_never_sent_type_4() {
    let mut wire = Wire::new(MIDI1);
    let g = MidiGroup::new(3).unwrap();
    let c = MidiChannel::new(5).unwrap();
    let out = convert(
        &mut wire,
        &[
            midi::note_on(g, c, 60, 0xFFFF),
            midi::note_off(g, c, 60, 0),
            midi::pitch_bend(g, c, 0x8000_0000),
            midi::per_note_pitch_bend(g, c, 60, 0x9000_0000),
            rpn(0, 0, 12 << 25),
            midi::program_change(g, c, 5, Some(0x0081)),
        ],
    );
    assert!(out.iter().all(|u| u.message_type() != 4), "{out:?}");
    let decoded: Vec<_> = out.iter().map(|&u| decode(u).unwrap()).collect();
    // The first three keep their Group and Channel, and the per-note bend,
    // which MIDI 1.0 cannot carry (D.2.8), is gone.
    assert_eq!(
        decoded[0],
        Message::Midi1(
            Address {
                group: g,
                channel: c
            },
            Voice1::NoteOn {
                note: NoteNumber::of(60),
                velocity: U7::of(127)
            }
        )
    );
    assert!(matches!(
        decoded[1],
        Message::Midi1(_, Voice1::NoteOff { .. })
    ));
    assert!(matches!(
        decoded[2],
        Message::Midi1(_, Voice1::PitchBend(_))
    ));
    // The RPN (4), then the Program Change with a bank (CC 0, CC 32, program).
    assert_eq!(decoded.len(), 3 + 4 + 3);
    let address_of = |m: &Message| match m {
        Message::Midi1(address, _) => *address,
        other => panic!("not a MIDI 1.0 channel message: {other:?}"),
    };
    assert!(decoded[3..7].iter().all(|m| address_of(m) == address(0, 0)));
    assert!(decoded[7..].iter().all(|m| address_of(m)
        == Address {
            group: g,
            channel: c
        }));
    assert!(matches!(
        decoded[9],
        Message::Midi1(_, Voice1::ProgramChange(_))
    ));
}

/// M2-104 Appendix D.2.3: a Registered Controller is "a sequence of four MIDI
/// 1.0 Protocol messages", CC 101, 100, 6 and 38, and the select is skipped for
/// the same parameter on the same channel (MIDI 1.0 rule 6), which is state of
/// the output's one narrower: the second value of one parameter is two
/// messages, a value of another is four again.
#[test]
fn a_registered_controller_is_four_control_changes_through_one_narrower() {
    let mut wire = Wire::new(MIDI1);
    // Pitch bend sensitivity (RPN 0/0), 12 semitones, zero-extended.
    let out = convert(&mut wire, &[rpn(0, 0, 12 << 25)]);
    assert_eq!(controls(&out), [(101, 0), (100, 0), (6, 12), (38, 0)]);
    let out = convert(&mut wire, &[rpn(0, 0, 2 << 25)]);
    assert_eq!(controls(&out), [(6, 2), (38, 0)], "the select is kept");
    let out = convert(&mut wire, &[rpn(0, 1, 0)]);
    assert_eq!(controls(&out).len(), 4, "another parameter selects again");
}

/// A MIDI 2.0 Protocol endpoint is sent the graph's messages as they are, type
/// 4 included, and a MIDI 1.0 Protocol one is sent MIDI 1.0 (type 2) as it is.
#[test]
fn a_midi2_endpoint_is_sent_the_packets_as_they_are() {
    let mut wire = Wire::new(MIDI2);
    let g = MidiGroup::FIRST;
    let c = MidiChannel::FIRST;
    let events = [
        midi::note_on(g, c, 60, 0xFFFF),
        rpn(0, 0, 12 << 25),
        midi::per_note_pitch_bend(g, c, 60, 0x9000_0000),
    ];
    assert_eq!(convert(&mut wire, &events), events);
    let mut wire = Wire::new(MIDI1);
    let m1 = midi::m1_note_on(c, NoteNumber::of(60), U7::of(100));
    assert_eq!(convert(&mut wire, &[m1]), [m1]);
}

/// System, SysEx7 and the Stream messages of the UMP Endpoint's own
/// conversation pass in either Protocol.
#[test]
fn system_sysex7_and_stream_messages_pass_in_either_protocol() {
    let sysex = Ump::from(Message::Sysex7(
        MidiGroup::FIRST,
        Sysex7Packet::new(SysexStatus::Complete, &[1, 2, 3]).unwrap(),
    ));
    let clock = Ump::from(Message::System(MidiGroup::FIRST, System::TimingClock));
    let discovery = Ump([0xF000_0101, 0x1F, 0, 0]);
    for config in [MIDI1, MIDI2] {
        let mut wire = Wire::new(config);
        let events = [sysex, clock, discovery];
        assert_eq!(convert(&mut wire, &events), events);
    }
}

/// M2-104 §7.1.6.2 (RXJR): the Sender "shall not send JR Timestamps until after
/// a Stream Configuration Notification message ... has been received". A JR
/// Clock and a JR Timestamp are dropped while the device's configuration does
/// not receive them, in either Protocol, and go after one that does; the music
/// is untouched either way.
#[test]
fn jr_goes_only_where_the_configuration_enables_it() {
    let jr = [
        Ump::from(Message::Utility(Utility::JrClock(100))),
        Ump::from(Message::Utility(Utility::JrTimestamp(200))),
    ];
    let note = midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xFFFF);
    for protocol in [Protocol::Midi1, Protocol::Midi2] {
        let off = StreamConfig {
            protocol,
            rx_jr: false,
            tx_jr: false,
        };
        let mut wire = Wire::new(off);
        let narrowed = convert(&mut wire, &[note]);
        let music = if protocol == Protocol::Midi2 {
            vec![note]
        } else {
            narrowed
        };
        assert_eq!(convert(&mut wire, &[jr[0], note, jr[1]]), music);
        wire.set_config(StreamConfig { rx_jr: true, ..off });
        let out = convert(&mut wire, &[jr[0], note, jr[1]]);
        assert_eq!(out.len(), 3, "{protocol:?}");
        assert_eq!((out[0], out[2]), (jr[0], jr[1]));
        // Receiving JR does not mean sending it: `tx_jr` changes nothing here.
        wire.set_config(StreamConfig {
            rx_jr: false,
            tx_jr: true,
            ..off
        });
        assert_eq!(convert(&mut wire, &[jr[0]]), []);
    }
}

/// The Delta Clockstamp and Ticks Per Quarter Note are a clip's (M2-104 §7.2.3
/// and §7.2.4), not a live wire's: never sent, with JR on or off.
#[test]
fn a_clips_utility_messages_are_never_sent() {
    let clip = [
        Ump::from(Message::Utility(Utility::TicksPerQuarterNote(480))),
        Ump::from(Message::Utility(Utility::DeltaClockstamp(
            tutti_ump::DeltaTicks::new(10).unwrap(),
        ))),
    ];
    let noop = Ump::from(Message::Utility(Utility::Noop));
    for rx_jr in [false, true] {
        let mut wire = Wire::new(StreamConfig { rx_jr, ..MIDI2 });
        assert_eq!(convert(&mut wire, &[clip[0], noop, clip[1]]), [noop]);
    }
}

/// The count back to the caller is of events: an event that became nothing was
/// taken, and one of four packets counts when the device took all four.
#[test]
fn the_accepted_count_is_in_the_callers_events() {
    let mut wire = Wire::new(MIDI1);
    let jr = Ump::from(Message::Utility(Utility::JrClock(1)));
    let note = midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xFFFF);
    let events: Vec<UmpAt> = [jr, rpn(0, 0, 0), note].into_iter().map(at).collect();
    wire.convert(&events);
    assert_eq!(wire.batch().len(), 5, "no JR, four for the RPN, one note");
    // The device took n of the five packets.
    let counts: Vec<usize> = (0..=5).map(|n| wire.accepted(n)).collect();
    // The JR is nothing, so it is taken with nothing taken.
    assert_eq!(counts, [1, 1, 1, 1, 2, 3]);
}

/// A device that goes from one Protocol to another has not seen what the
/// narrower believes it was told: the next controller selects in full.
#[test]
fn a_change_of_protocol_makes_the_narrower_forget() {
    let mut wire = Wire::new(MIDI1);
    convert(&mut wire, &[rpn(0, 0, 0)]);
    assert_eq!(controls(&convert(&mut wire, &[rpn(0, 0, 0)])).len(), 2);
    wire.set_config(MIDI2);
    wire.set_config(MIDI1);
    assert_eq!(controls(&convert(&mut wire, &[rpn(0, 0, 0)])).len(), 4);
    // A change of JR alone does not.
    wire.set_config(StreamConfig {
        rx_jr: true,
        ..MIDI1
    });
    assert_eq!(controls(&convert(&mut wire, &[rpn(0, 0, 0)])).len(), 2);
}
