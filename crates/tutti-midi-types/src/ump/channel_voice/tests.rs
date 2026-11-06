use super::*;
use midi2::{UmpMessage, channel_voice2};

#[test]
fn note_on_decodes_via_midi2() {
    let ev = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::new(5), 60, 0x8000);
    let msg = UmpMessage::try_from(ev.data_words()).unwrap();
    match msg {
        UmpMessage::ChannelVoice2(channel_voice2::ChannelVoice2::NoteOn(m)) => {
            assert_eq!(u8::from(m.channel()), 5);
            assert_eq!(u8::from(m.note_number()), 60);
            assert_eq!(m.velocity(), 0x8000);
        }
        _ => panic!("expected CV2 NoteOn, got {msg:?}"),
    }
}

#[test]
fn cc_decodes_via_midi2() {
    let ev = MidiEvent::cc(
        MidiGroup::FIRST,
        MidiChannel::new(2),
        CCNumber::BRIGHTNESS,
        0xDEAD_BEEF,
    );
    let msg = UmpMessage::try_from(ev.data_words()).unwrap();
    let UmpMessage::ChannelVoice2(channel_voice2::ChannelVoice2::ControlChange(m)) = msg else {
        panic!("expected CV2 ControlChange");
    };
    assert_eq!(u8::from(m.channel()), 2);
    assert_eq!(u8::from(m.control()), 74);
    assert_eq!(m.control_change_data(), 0xDEAD_BEEF);
}

/// Every CC number survives the trip out to the wire and back. The mask
/// happens once, at `CCNumber::new`, and nothing downstream can change the
/// number (a debug-only assert plus a mask at the wire would let a release
/// build silently emit a *different* controller).
#[test]
fn every_cc_number_round_trips_through_the_wire() {
    for raw in 0u8..128 {
        let n = CCNumber::new(raw);
        let ev = MidiEvent::cc(MidiGroup::FIRST, MidiChannel::new(2), n, 0);
        let msg = UmpMessage::try_from(ev.data_words()).unwrap();
        let UmpMessage::ChannelVoice2(channel_voice2::ChannelVoice2::ControlChange(m)) = msg else {
            panic!("expected CV2 ControlChange");
        };
        assert_eq!(CCNumber::new(u8::from(m.control())), n, "CC {raw}");
    }
}

/// The channel and the CC number do not collide. They sit adjacent in
/// `cc(group, channel, cc, value)`, and as bare `u8`s a swap would emit a
/// well-formed packet addressing the wrong controller on the wrong channel.
/// The newtypes make that a compile error; this pins the property the
/// compile error protects — that the two land in *different* wire fields.
#[test]
fn the_channel_and_the_cc_number_land_in_different_fields() {
    // Deliberately equal raw values, so a field mix-up cannot hide behind
    // one of them: only ordering distinguishes correct from swapped.
    let ev = MidiEvent::cc(
        MidiGroup::FIRST,
        MidiChannel::new(7),
        CCNumber::new(3),
        0x1234_5678,
    );
    let msg = UmpMessage::try_from(ev.data_words()).unwrap();
    let UmpMessage::ChannelVoice2(channel_voice2::ChannelVoice2::ControlChange(m)) = msg else {
        panic!("expected CV2 ControlChange");
    };
    assert_eq!(u8::from(m.channel()), 7, "channel");
    assert_eq!(u8::from(m.control()), 3, "cc number");
}

#[test]
fn per_note_management_decodes_via_midi2() {
    let ev = MidiEvent::per_note_management(MidiGroup::FIRST, MidiChannel::new(4), 60, true, false);
    let msg = UmpMessage::try_from(ev.data_words()).unwrap();
    let UmpMessage::ChannelVoice2(channel_voice2::ChannelVoice2::PerNoteManagement(m)) = msg else {
        panic!("expected CV2 PerNoteManagement");
    };
    assert_eq!(u8::from(m.channel()), 4);
    assert_eq!(u8::from(m.note_number()), 60);
    assert!(m.detach());
    assert!(!m.reset());
}
