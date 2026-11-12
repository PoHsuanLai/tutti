//! `NoteVelocity`: the whole 16-bit field of a MIDI 2.0 Note On or Note Off,
//! zero included (M2-104 section 7.4.2).

use tutti_ump::{Address, Message, MidiChannel, NoteNumber, NoteVelocity, Ump, Voice2};

/// Every `u16` is a velocity: `new` keeps it and `get` gives it back, and the
/// bounds are the field's ends.
#[test]
fn the_whole_field_is_a_velocity() {
    for raw in [0, 1, 0x7FFF, 0x8000, 0xFFFE, 0xFFFF] {
        let v = NoteVelocity::new(raw);
        assert_eq!(v.get(), raw);
        assert_eq!(u16::from(v), raw);
    }
    assert_eq!(NoteVelocity::MIN.get(), 0);
    assert_eq!(NoteVelocity::MAX.get(), 0xFFFF);
}

/// The velocity sits in the top 16 bits of the packet's second word, through a
/// Note On and a Note Off, and decodes back as the same `NoteVelocity` (a Note
/// On at velocity 0 stays a Note On).
#[test]
fn a_velocity_survives_encode_and_decode() {
    let address = Address::first(MidiChannel::of(5));
    let note = NoteNumber::of(64);
    for raw in [0, 1, 0x00FF, 0x8000, 0xABCD, 0xFFFF] {
        let v = NoteVelocity::new(raw);
        for voice in [Voice2::note_on(note, v), Voice2::note_off(note, v)] {
            let ump = Ump::from(Message::Midi2(address, voice));
            assert_eq!(ump.0[1], u32::from(raw) << 16, "{raw:#06x}");
            assert_eq!(
                ump.decode().unwrap(),
                Message::Midi2(address, voice),
                "{raw:#06x}"
            );
        }
    }
}

/// The plain derive reads and writes the bare number (the one type in the
/// crate whose every raw is valid).
#[cfg(feature = "serde")]
#[test]
fn it_is_written_as_the_bare_number() {
    let v = NoteVelocity::new(0xABCD);
    let text = serde_json::to_string(&v).unwrap();
    assert_eq!(text, "43981");
    assert_eq!(serde_json::from_str::<NoteVelocity>(&text).unwrap(), v);
}
