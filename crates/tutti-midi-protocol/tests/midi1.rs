//! `narrow` and `midi1`: a message at MIDI 1.0.
//!
//! The expected bytes are written out from M2-104 Appendix D and M2-115, or
//! follow from `widen` and the figures `tests/narrow.rs` pins.
//!
//! Mutation record (each planted in `src/midi1.rs`, seen to fail, restored):
//! - `NOTE_ON_FLOOR` removed from the Note On arm of `narrow` ->
//!   `a_note_on_never_narrows_to_velocity_zero`;
//! - the same floor applied to Note Off too -> `a_note_off_keeps_a_zero_velocity`
//!   and `narrow_returns_what_widen_was_given`;
//! - the bank LSB sent as CC 33 (in `Bank::bytes`), the Program Change sent
//!   before the bank, and the bank not sent -> `a_bank_is_three_messages`;
//! - a per-note pitch bend narrowed to a channel pitch bend ->
//!   `messages_without_a_midi1_form_send_nothing`.

use tutti_midi_protocol::{midi1, narrow, widen};
use tutti_ump::{
    Address, Attribute, Bank, CCNumber, Message, MidiChannel, MidiGroup, NoteNumber,
    RegisteredPerNoteController, Sysex7Packet, SysexStatus, System, U7, U14, Voice1, Voice2,
};

fn address(channel: u8) -> Address {
    Address {
        group: MidiGroup::FIRST,
        channel: MidiChannel::of(channel),
    }
}

fn bytes(message: Message) -> Vec<Vec<u8>> {
    midi1(message).map(|b| b.as_slice().to_vec()).collect()
}

/// M2-104 section 7.4.2: "if the translated MIDI 1.0 value of the Velocity is
/// zero, then the Translator shall replace the zero with a value of 1." Every
/// 16-bit velocity that narrows to 0, and 0 itself.
#[test]
fn a_note_on_never_narrows_to_velocity_zero() {
    for velocity in 0..=u16::MAX {
        let Some(Voice1::NoteOn { velocity: got, .. }) = narrow(Voice2::NoteOn {
            note: NoteNumber::of(60),
            velocity,
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
            velocity,
            attribute: Attribute::None,
        });
        assert_eq!(
            off,
            Some(Voice1::NoteOff {
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
                velocity: 0x8000,
                attribute,
            },
        );
        assert_eq!(bytes(on), [[0x92, 60, 64]], "{attribute:?}");
        let off = Message::Midi2(
            address(2),
            Voice2::NoteOff {
                note: NoteNumber::of(60),
                velocity: 0x8000,
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
                value: 0x8000_0000
            }
        )),
        [[0xB1, 7, 64]]
    );
    assert_eq!(
        bytes(Message::Midi2(address(1), Voice2::PitchBend(0x8000_0000))),
        [[0xE1, 0x00, 0x40]]
    );
    assert_eq!(
        bytes(Message::Midi2(
            address(1),
            Voice2::ChannelPressure(u32::MAX)
        )),
        [[0xD1, 127]]
    );
    assert_eq!(
        bytes(Message::Midi2(
            address(1),
            Voice2::PolyPressure {
                note: NoteNumber::of(60),
                value: 0
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
            value: 0x8000_0000,
        },
        Voice2::RegisteredPerNote {
            note: NoteNumber::of(60),
            controller: RegisteredPerNoteController::Volume,
            value: 1,
        },
        Voice2::Registered {
            bank: U7::of(0),
            index: U7::of(0),
            value: 0,
        },
        Voice2::PerNoteManagement {
            note: NoteNumber::of(60),
            detach: true,
            reset: false,
        },
    ];
    for voice in none {
        assert_eq!(narrow(voice), None, "{voice:?}");
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
                assert_eq!(narrow(widen(voice)), Some(voice), "{voice:?}");
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
            assert_eq!(narrow(widen(on)), Some(want), "{on:?}");
        }
    }
}
