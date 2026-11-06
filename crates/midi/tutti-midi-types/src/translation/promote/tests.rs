use super::*;

/// Assert `ev` decodes to a specific CV2 variant, running `check` on it.
macro_rules! expect_cv2 {
    ($ev:expr, $pat:pat => $check:block) => {{
        match UmpMessage::try_from($ev.data_words()).unwrap() {
            UmpMessage::ChannelVoice2($pat) => $check,
            other => panic!("expected {}, got {other:?}", stringify!($pat)),
        }
    }};
}

#[test]
fn cv1_note_on_promotes_to_cv2_full_width() {
    let norm = normalize(&MidiEvent::from_midi1_bytes(0, &[0x93, 60, 100]).unwrap());
    expect_cv2!(norm, ChannelVoice2::NoteOn(m) => {
        assert_eq!(u8::from(m.channel()), 3);
        assert_eq!(u8::from(m.note_number()), 60);
        assert_eq!(m.velocity(), midi1_velocity_to_midi2(100)); // widened, not <<9
    });
}

// Mutation: pass 0 instead of the scaled velocity and this fails.
#[test]
fn cv1_note_off_keeps_release_velocity() {
    let norm = normalize(&MidiEvent::from_midi1_bytes(0, &[0x80, 60, 64]).unwrap());
    expect_cv2!(norm, ChannelVoice2::NoteOff(m) => {
        assert_eq!(m.velocity(), 0x8000);
    });
}

#[test]
fn cv1_note_on_velocity_zero_folds_to_note_off() {
    let norm = normalize(&MidiEvent::from_midi1_bytes(0, &[0x93, 60, 0]).unwrap());
    expect_cv2!(norm, ChannelVoice2::NoteOff(m) => { assert_eq!(u8::from(m.note_number()), 60); });
}

#[test]
fn cv2_note_on_velocity_zero_folds_to_note_off() {
    let norm = normalize(&MidiEvent::note_on(
        MidiGroup::FIRST,
        MidiChannel::new(3),
        60,
        0,
    ));
    expect_cv2!(norm, ChannelVoice2::NoteOff(m) => { assert_eq!(u8::from(m.channel()), 3); });
}

#[test]
fn cv2_passthrough_preserves_frame_offset() {
    let norm = normalize(
        &MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::new(1), 64, 0x8000)
            .with_frame_offset(128),
    );
    assert_eq!(norm.frame_offset, 128);
    expect_cv2!(norm, ChannelVoice2::NoteOn(m) => { assert_eq!(m.velocity(), 0x8000); });
}
