//! `narrow` and `midi1`: a message at MIDI 1.0.
//!
//! The expected bytes are written out from M2-104 Appendix D and M2-115, or
//! follow from `widen` and the figures `tests/narrow.rs` pins.

use tutti_midi_protocol::{NoMidi1Form, midi1, narrow, widen};
use tutti_ump::{
    Address, Attribute, Bank, CCNumber, Message, MidiChannel, MidiGroup, NoteNumber, NoteVelocity,
    RegisteredPerNoteController, Sysex7Packet, SysexStatus, System, U7, U14, Voice1, Voice2,
};
use tutti_ump::{BendValue, ControlValue};

fn address(channel: u8) -> Address {
    Address {
        group: MidiGroup::FIRST,
        channel: MidiChannel::of(channel),
    }
}

fn bytes(message: Message) -> Vec<Vec<u8>> {
    midi1(message).map(|b| b.as_slice().to_vec()).collect()
}

/// M2-104 section 7.4.2: a translated velocity that would be
/// zero is replaced by 1. Every
/// 16-bit velocity that narrows to 0, and 0 itself.
/// Spec: m2-104/7.4.2#1
#[test]
fn a_note_on_never_narrows_to_velocity_zero() {
    for velocity in 0..=u16::MAX {
        let Ok(Voice1::NoteOn { velocity: got, .. }) = narrow(Voice2::NoteOn {
            note: NoteNumber::of(60),
            velocity: NoteVelocity::new(velocity),
            attribute: Attribute::None,
        }) else {
            panic!("{velocity:#x}");
        };
        assert_eq!(
            u16::from(got.get()),
            (velocity >> 9).max(1),
            "{velocity:#x}"
        );
    }
}

/// A Note Off at velocity 0 is a Note Off at release velocity 0: the rule is
/// the Note On's.
#[test]
fn a_note_off_keeps_a_zero_velocity() {
    for (velocity, want) in [(0, 0), (0x01FF, 0), (0x0200, 1), (0xFFFF, 127)] {
        let off = narrow(Voice2::NoteOff {
            note: NoteNumber::of(60),
            velocity: NoteVelocity::new(velocity),
            attribute: Attribute::None,
        });
        assert_eq!(
            off,
            Ok(Voice1::NoteOff {
                note: NoteNumber::of(60),
                velocity: U7::of(want)
            }),
            "{velocity:#x}"
        );
    }
}

/// The attribute does not exist in MIDI 1.0: a note with one, reserved types
/// included, still sends its bytes.
#[test]
fn a_note_with_any_attribute_type_sends_bytes() {
    let attributes = [
        Attribute::None,
        Attribute::Manufacturer(0x1234),
        Attribute::Profile(7),
        Attribute::Pitch(0x3C00),
        Attribute::Reserved { kind: 4, data: 1 },
        Attribute::Reserved {
            kind: 255,
            data: 0xFFFF,
        },
    ];
    for attribute in attributes {
        let on = Message::Midi2(
            address(2),
            Voice2::NoteOn {
                note: NoteNumber::of(60),
                velocity: NoteVelocity::new(0x8000),
                attribute,
            },
        );
        assert_eq!(bytes(on), [[0x92, 60, 64]], "{attribute:?}");
        let off = Message::Midi2(
            address(2),
            Voice2::NoteOff {
                note: NoteNumber::of(60),
                velocity: NoteVelocity::new(0x8000),
                attribute,
            },
        );
        assert_eq!(bytes(off), [[0x82, 60, 64]], "{attribute:?}");
    }
}

/// M2-104 Appendix D: a Program Change with a valid bank is Bank Select MSB
/// (CC 0), Bank Select LSB (CC 32), then the Program Change.
#[test]
fn a_bank_is_three_messages() {
    let banked = Message::Midi2(
        address(5),
        Voice2::ProgramChange {
            program: U7::of(9),
            bank: Some(Bank {
                msb: U7::of(3),
                lsb: U7::of(4),
            }),
        },
    );
    assert_eq!(
        bytes(banked),
        [vec![0xB5, 0, 3], vec![0xB5, 32, 4], vec![0xC5, 9]]
    );
    let plain = Message::Midi2(
        address(5),
        Voice2::ProgramChange {
            program: U7::of(9),
            bank: None,
        },
    );
    assert_eq!(bytes(plain), [[0xC5, 9]]);
}

/// MIDI 1.0 voice and System messages are their own bytes; a MIDI 2.0
/// controller, pressure and bend narrow to the top bits.
#[test]
fn the_other_messages_are_their_bytes() {
    let channel = MidiChannel::of(1);
    let cc = Voice1::ControlChange {
        controller: CCNumber::of(7),
        value: U7::of(100),
    };
    assert_eq!(
        bytes(Message::Midi1(address(1), cc)),
        [cc.bytes(channel).as_slice()]
    );
    assert_eq!(
        bytes(Message::System(
            MidiGroup::FIRST,
            System::SongPosition(U14::of(0x101))
        )),
        [[0xF2, 0x01, 0x02]]
    );
    assert_eq!(
        bytes(Message::Midi2(
            address(1),
            Voice2::ControlChange {
                controller: CCNumber::of(7),
                value: ControlValue::new(0x8000_0000)
            }
        )),
        [[0xB1, 7, 64]]
    );
    assert_eq!(
        bytes(Message::Midi2(
            address(1),
            Voice2::PitchBend(BendValue::new(0x8000_0000))
        )),
        [[0xE1, 0x00, 0x40]]
    );
    assert_eq!(
        bytes(Message::Midi2(
            address(1),
            Voice2::ChannelPressure(ControlValue::new(u32::MAX))
        )),
        [[0xD1, 127]]
    );
    assert_eq!(
        bytes(Message::Midi2(
            address(1),
            Voice2::PolyPressure {
                note: NoteNumber::of(60),
                value: ControlValue::new(0)
            }
        )),
        [[0xA1, 60, 0]]
    );
}

#[test]
fn messages_without_a_midi1_form_send_nothing() {
    let none = [
        Voice2::PerNotePitchBend {
            note: NoteNumber::of(60),
            value: BendValue::new(0x8000_0000),
        },
        Voice2::RegisteredPerNote {
            note: NoteNumber::of(60),
            controller: RegisteredPerNoteController::Volume,
            value: ControlValue::new(1),
        },
        Voice2::Registered {
            bank: U7::of(0),
            index: U7::of(0),
            value: ControlValue::new(0),
        },
        Voice2::PerNoteManagement {
            note: NoteNumber::of(60),
            detach: true,
            reset: false,
        },
    ];
    for voice in none {
        assert!(narrow(voice).is_err(), "{voice:?}");
        assert!(
            bytes(Message::Midi2(address(0), voice)).is_empty(),
            "{voice:?}"
        );
    }
    let packet = Sysex7Packet::new(SysexStatus::Complete, &[1, 2]).unwrap();
    assert!(bytes(Message::Sysex7(MidiGroup::FIRST, packet)).is_empty());
}

/// Widening a MIDI 1.0 voice message and narrowing it back returns it, for
/// every value on every channel, except the Note On at velocity 0 that
/// `widen` turns into a Note Off (Appendix D.3.1).
/// Spec: m2-104/d.1.1#2, m2-104/d.1.2#1
#[test]
fn narrow_returns_what_widen_was_given() {
    for d1 in 0..128u8 {
        for d2 in 0..128u8 {
            let (note, v) = (NoteNumber::of(d1), U7::of(d2));
            let voices = [
                Voice1::NoteOff { note, velocity: v },
                Voice1::PolyPressure { note, pressure: v },
                Voice1::ControlChange {
                    controller: CCNumber::of(d1),
                    value: v,
                },
                Voice1::ProgramChange(U7::of(d1)),
                Voice1::ChannelPressure(U7::of(d1)),
                Voice1::PitchBend(U14::of(u16::from(d1) << 7 | u16::from(d2))),
            ];
            for voice in voices {
                assert_eq!(narrow(widen(voice)), Ok(voice), "{voice:?}");
            }
            let on = Voice1::NoteOn { note, velocity: v };
            let want = if d2 == 0 {
                Voice1::NoteOff {
                    note,
                    velocity: U7::of(64),
                }
            } else {
                on
            };
            assert_eq!(narrow(widen(on)), Ok(want), "{on:?}");
        }
    }
}

/// M2-104 Appendix D.2.8: per-note controllers, registered and assignable
/// controllers and per-note management have no single MIDI 1.0 message, and
/// the refusal says which of them it was.
#[test]
fn a_refusal_names_why_there_is_no_midi1_message() {
    let note = NoteNumber::of(60);
    assert_eq!(
        narrow(Voice2::PerNotePitchBend {
            note,
            value: BendValue::new(0)
        }),
        Err(NoMidi1Form::PerNote)
    );
    assert_eq!(
        narrow(Voice2::Registered {
            bank: U7::of(0),
            index: U7::of(0),
            value: ControlValue::new(0)
        }),
        Err(NoMidi1Form::Parameter)
    );
    assert_eq!(
        narrow(Voice2::PerNoteManagement {
            note,
            detach: true,
            reset: false
        }),
        Err(NoMidi1Form::PerNoteManagement)
    );
}

/// `narrow_message` is `narrow` on the voice with the Group and Channel kept
/// (M2-104 Appendix D.2): a MIDI 2.0 Note On on Group 2, Channel 5 is the MIDI
/// 1.0 Note On there, and a message that is not MIDI 2.0 channel voice is as
/// it came.
#[test]
fn narrow_message_keeps_the_address() {
    use tutti_midi_protocol::{narrow_message, widen_velocity};
    let to = Address::new(MidiGroup::of(2), MidiChannel::of(5));
    let note = NoteNumber::of(60);
    let on2 = Message::Midi2(to, Voice2::note_on(note, widen_velocity(U7::of(100))));
    assert_eq!(
        narrow_message(on2),
        Ok(Message::midi1(to, Voice1::note_on(note, U7::of(100))))
    );
    let clock = Message::System(MidiGroup::of(2), System::TimingClock);
    assert_eq!(narrow_message(clock), Ok(clock));
    let already = Message::midi1(to, Voice1::note_on(note, U7::of(9)));
    assert_eq!(narrow_message(already), Ok(already));
}

/// A message MIDI 1.0 has no single message for is refused with the reason
/// `narrow` gives, not dropped: a Per-Note Pitch Bend (D.2.8).
#[test]
fn narrow_message_refuses_what_narrow_refuses() {
    use tutti_midi_protocol::narrow_message;
    let bend = Message::Midi2(
        address(0),
        Voice2::PerNotePitchBend {
            note: NoteNumber::of(60),
            value: BendValue::CENTER,
        },
    );
    assert_eq!(narrow_message(bend), Err(NoMidi1Form::PerNote));
}
