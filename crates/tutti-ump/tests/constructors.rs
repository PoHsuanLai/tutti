//! The constructors that spell a packet's parts: `Address::first`,
//! `Voice2::note_on` and `note_off`, and the Timing Clock rate.

use tutti_ump::{
    Address, Attribute, Message, MidiChannel, MidiGroup, NoteNumber, System, Ump, Voice2, decode,
};

/// Mutation (run): `Address::first` using `MidiGroup::LAST` fails the group.
#[test]
fn first_is_the_first_group_with_the_channel() {
    for c in 0..16 {
        let channel = MidiChannel::of(c);
        let a = Address::first(channel);
        assert_eq!(a.group, MidiGroup::of(0));
        assert_eq!(a.channel, channel);
    }
}

/// M2-104 section 7.4.2: Note On and Note Off carry a 16-bit velocity and an
/// attribute type, and "no attribute" is type 0. Mutation (run): swapping the
/// two constructors' variants fails the decode; a nonzero attribute fails the
/// equality.
#[test]
fn note_constructors_are_the_no_attribute_messages() {
    let note = NoteNumber::of(60);
    assert_eq!(
        Voice2::note_on(note, 0x8000),
        Voice2::NoteOn {
            note,
            velocity: 0x8000,
            attribute: Attribute::None
        }
    );
    assert_eq!(
        Voice2::note_off(note, 0x1234),
        Voice2::NoteOff {
            note,
            velocity: 0x1234,
            attribute: Attribute::None
        }
    );
    // On the wire: group 0, channel 3, note 60, no attribute (Table 4's words).
    let address = Address::first(MidiChannel::of(3));
    let on = Ump::from(Message::Midi2(address, Voice2::note_on(note, 0x8000)));
    assert_eq!(on, Ump([0x4093_3C00, 0x8000_0000, 0, 0]));
    let off = Ump::from(Message::Midi2(address, Voice2::note_off(note, 0x1234)));
    assert_eq!(off, Ump([0x4083_3C00, 0x1234_0000, 0, 0]));
    assert!(matches!(
        decode(on),
        Ok(Message::Midi2(
            _,
            Voice2::NoteOn {
                velocity: 0x8000,
                ..
            }
        ))
    ));
}

/// MIDI 1.0 Detailed Specification: 24 Timing Clocks per quarter note.
/// Mutation (run): any other value fails.
#[test]
fn a_quarter_note_is_24_timing_clocks() {
    assert_eq!(System::TIMING_CLOCKS_PER_QUARTER, 24);
}
