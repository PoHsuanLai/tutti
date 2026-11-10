use midi2::{UmpMessage, system_common};
use tutti_fixtures::midi;
use tutti_ump::MidiGroup;

#[test]
fn timing_clock_is_one_word() {
    let ev = midi::timing_clock(MidiGroup::FIRST);
    assert_eq!(ev.words().len(), 1);
    let msg = UmpMessage::try_from(ev.words()).unwrap();
    assert!(matches!(
        msg,
        UmpMessage::SystemCommon(system_common::SystemCommon::TimingClock(_))
    ));
}

#[test]
fn song_position_round_trips_14bit() {
    let ev = midi::song_position(MidiGroup::FIRST, 12345);
    let msg = UmpMessage::try_from(ev.words()).unwrap();
    let UmpMessage::SystemCommon(system_common::SystemCommon::SongPositionPointer(m)) = msg else {
        panic!("expected SongPositionPointer");
    };
    assert_eq!(u16::from(m.position()), 12345);
}
