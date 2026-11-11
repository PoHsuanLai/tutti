use super::*;
use tutti_ump::{MidiChannel, MidiGroup};

#[test]
fn create_ump_source() {
    let src = UmpVirtualSource::new("Test UMP Source").expect("creates");
    assert_eq!(src.name(), "Test UMP Source");
}

#[test]
fn send_empty_is_noop() {
    let src = UmpVirtualSource::new("Test UMP Empty").expect("creates");
    assert!(src.send_ump(&[]).is_ok());
}

#[test]
fn send_jr_timestamp_word() {
    // A JR Timestamp is a single UMP Utility word — the whole point of this
    // endpoint (it has no MIDI 1.0 form). It must send without error.
    let src = UmpVirtualSource::new("Test UMP JR").expect("creates");
    let jr = tutti_fixtures::midi::jr_timestamp(0x1234).words()[0];
    assert!(src.send_ump(&[jr]).is_ok());
}

#[test]
fn send_multiword_note_on() {
    // A MIDI 2.0 channel-voice note-on is two words.
    let src = UmpVirtualSource::new("Test UMP Note").expect("creates");
    let note = tutti_fixtures::midi::note_on(
        MidiGroup::FIRST,
        MidiChannel::FIRST,
        60,
        tutti_fixtures::midi::MID_VELOCITY,
    );
    assert!(src.send_ump(note.words()).is_ok());
}
