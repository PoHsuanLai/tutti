use super::*;
use crate::from_midi1_bytes;
use tutti_fixtures::midi;

/// Assert `ev` decodes to a specific CV2 variant, running `check` on it.
macro_rules! expect_cv2 {
    ($ev:expr, $pat:pat => $check:block) => {{
        match UmpMessage::try_from($ev.words()).unwrap() {
            UmpMessage::ChannelVoice2($pat) => $check,
            other => panic!("expected {}, got {other:?}", stringify!($pat)),
        }
    }};
}

#[test]
fn cv1_note_on_promotes_to_cv2_full_width() {
    let norm = normalize(&from_midi1_bytes(&[0x93, 60, 100]).unwrap());
    expect_cv2!(norm, ChannelVoice2::NoteOn(m) => {
        assert_eq!(u8::from(m.channel()), 3);
        assert_eq!(u8::from(m.note_number()), 60);
        assert_eq!(m.velocity(), midi1_velocity_to_midi2(100)); // widened, not <<9
    });
}

// Mutation: pass 0 instead of the scaled velocity and this fails.
#[test]
fn cv1_note_off_keeps_release_velocity() {
    let norm = normalize(&from_midi1_bytes(&[0x80, 60, 64]).unwrap());
    expect_cv2!(norm, ChannelVoice2::NoteOff(m) => {
        assert_eq!(m.velocity(), 0x8000);
    });
}

#[test]
fn cv1_note_on_velocity_zero_folds_to_note_off() {
    let norm = normalize(&from_midi1_bytes(&[0x93, 60, 0]).unwrap());
    expect_cv2!(norm, ChannelVoice2::NoteOff(m) => { assert_eq!(u8::from(m.note_number()), 60); });
}

#[test]
fn cv2_note_on_velocity_zero_folds_to_note_off() {
    let norm = normalize(&midi::note_on(MidiGroup::FIRST, MidiChannel::of(3), 60, 0));
    expect_cv2!(norm, ChannelVoice2::NoteOff(m) => { assert_eq!(u8::from(m.channel()), 3); });
}

#[test]
fn cv2_passthrough_returns_the_packet_unchanged() {
    // (The packet carries no frame offset now: the caller pairs one, so what is
    // left to pin is that a CV2 note-on passes through bit for bit.)
    let note_on = midi::note_on(MidiGroup::FIRST, MidiChannel::of(1), 64, 0x8000);
    let norm = normalize(&note_on);
    assert_eq!(norm, note_on);
    expect_cv2!(norm, ChannelVoice2::NoteOn(m) => { assert_eq!(m.velocity(), 0x8000); });
}
