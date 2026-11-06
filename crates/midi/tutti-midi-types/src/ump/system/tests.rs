use super::*;
use midi2::{UmpMessage, system_common};

#[test]
fn timing_clock_is_one_word() {
    let ev = MidiEvent::timing_clock(MidiGroup::FIRST);
    assert_eq!(ev.data_words().len(), 1);
    let msg = UmpMessage::try_from(ev.data_words()).unwrap();
    assert!(matches!(
        msg,
        UmpMessage::SystemCommon(system_common::SystemCommon::TimingClock(_))
    ));
}

#[test]
fn song_position_round_trips_14bit() {
    let ev = MidiEvent::song_position(MidiGroup::FIRST, 12345);
    let msg = UmpMessage::try_from(ev.data_words()).unwrap();
    let UmpMessage::SystemCommon(system_common::SystemCommon::SongPositionPointer(m)) = msg else {
        panic!("expected SongPositionPointer");
    };
    assert_eq!(u16::from(m.position()), 12345);
}
