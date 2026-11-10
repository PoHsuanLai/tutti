use super::*;
use tutti_fixtures::midi;
use tutti_ump::MidiChannel;

#[test]
fn try_from_bytes_parses_and_errors() {
    // Note-on wire bytes → a valid Ump.
    let ev = parse_midi1_bytes(&[0x93u8, 60, 100]).expect("valid note-on");
    assert_eq!(ev, from_midi1_bytes(&[0x93, 60, 100]).unwrap());
    // Garbage → the typed error.
    assert_eq!(parse_midi1_bytes(&[0x00u8]), Err(MidiParseError));
}

// Mutation: drop the bank branch in `to_midi1_messages` and the first two
// messages vanish.
#[test]
fn program_change_with_a_bank_is_three_midi1_messages() {
    let ev = midi::program_change(MidiGroup::FIRST, MidiChannel::of(2), 5, Some(0x0123));
    let msgs: Vec<_> = to_midi1_messages(&ev).collect();
    assert_eq!(
        msgs,
        vec![
            ([0xB2, 0, 0x02], 3),
            ([0xB2, 32, 0x23], 3),
            ([0xC2, 5, 0], 2)
        ]
    );
    // The single-message view stays the Program Change.
    assert_eq!(to_midi1_bytes(&ev), Some(([0xC2, 5, 0], 2)));
    // No bank: one message. No 1.0 form: none.
    let plain = midi::program_change(MidiGroup::FIRST, MidiChannel::of(2), 5, None);
    assert_eq!(to_midi1_messages(&plain).count(), 1);
    let rpn = midi::registered_controller(MidiGroup::FIRST, MidiChannel::FIRST, 0, 0, 0);
    assert_eq!(to_midi1_messages(&rpn).count(), 0);
}

#[test]
fn quiet_note_on_never_downscales_to_a_note_off() {
    // M2-104-UM §7.4.2: a CV2 Note On whose velocity narrows to 0 must be
    // sent as 1, because MIDI 1.0 reads velocity 0 as a Note Off. The
    // downscale is `>> 9`, so the whole 0x0001..=0x01FF range is at risk.
    for vel in [0x0001u16, 0x0080, 0x00FF, 0x01FF] {
        let ev = midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, vel);
        let (bytes, len) = to_midi1_bytes(&ev).expect("note-on downconverts");
        assert_eq!(len, 3);
        assert_eq!(bytes[0], 0x90, "status stays Note On");
        assert_eq!(
            bytes[2], 1,
            "velocity {vel:#06x} must clamp up to 1, not become a Note Off"
        );
    }

    // A velocity of exactly 0 is not a valid CV2 Note On gesture, but if one
    // arrives it must still not turn into a Note Off.
    let ev = midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0);
    assert_eq!(to_midi1_bytes(&ev).unwrap().0[2], 1);

    // Audible velocities are untouched.
    let ev = midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000);
    assert_eq!(to_midi1_bytes(&ev).unwrap().0[2], 64);

    // Note Off velocity 0 is legitimate and must NOT be clamped.
    let off = midi::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0);
    let (bytes, _) = to_midi1_bytes(&off).expect("note-off downconverts");
    assert_eq!(bytes[0], 0x80);
    assert_eq!(bytes[2], 0, "note-off velocity 0 is valid");
}
