//! `ControlValue` and `BendValue`: the 32-bit data word of a MIDI 2.0 message,
//! whose whole range is the raw.

use tutti_ump::{BendValue, CCNumber, ControlValue, Message, MidiChannel, NoteNumber, Ump, Voice2};

/// The Pitch Bend data word is `0x8000_0000` at the centre (M2-104 section
/// 7.4.11), the lowest bend is 0 and the highest is `0xFFFF_FFFF`.
#[test]
fn the_bend_centre_is_the_middle_of_the_word() {
    assert_eq!(BendValue::MIN.get(), 0);
    assert_eq!(BendValue::CENTER.get(), 0x8000_0000);
    assert_eq!(BendValue::MAX.get(), u32::MAX);
    let word = |bend| Ump::from(Message::pitch_bend2(MidiChannel::FIRST, bend)).0[1];
    assert_eq!(word(BendValue::CENTER), 0x8000_0000);
}

/// A 32-bit controller value is the whole data word of a Control Change (M2-104
/// section 7.4.6), and every `u32` is one: nothing is refused or clamped.
#[test]
fn every_word_is_a_control_value() {
    for raw in [0, 1, 0x7FFF_FFFF, 0x8000_0000, u32::MAX] {
        let packet = Ump::from(Message::control_change2(
            MidiChannel::FIRST,
            CCNumber::VOLUME,
            ControlValue::new(raw),
        ));
        assert_eq!(packet.0[1], raw);
        let Ok(Message::Midi2(_, Voice2::ControlChange { value, .. })) = packet.decode() else {
            panic!("a Control Change decodes as one");
        };
        assert_eq!(u32::from(value), raw);
    }
    assert_eq!(ControlValue::CENTER.get(), 0x8000_0000);
}

/// A Per-Note Pitch Bend carries a bend, not a controller value (M2-104 section
/// 7.4.7): it decodes into a `BendValue`.
#[test]
fn a_per_note_bend_decodes_to_a_bend() {
    let packet = Ump::from(Message::Midi2(
        MidiChannel::FIRST.into(),
        Voice2::PerNotePitchBend {
            note: NoteNumber::of(60),
            value: BendValue::new(0x9000_0000),
        },
    ));
    let Ok(Message::Midi2(_, Voice2::PerNotePitchBend { value, .. })) = packet.decode() else {
        panic!("a Per-Note Pitch Bend decodes as one");
    };
    assert_eq!(value, BendValue::new(0x9000_0000));
}
