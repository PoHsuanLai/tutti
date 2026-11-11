use super::*;
use tutti_ump::MidiGroup;

fn set_tempo(field: u32) -> Message {
    Message::Flex(MidiGroup::FIRST, Flex::SetTempo(field))
}

/// M2-104 section 7.5.3: the field is 10 ns units per quarter note, so 120 bpm
/// (0.5 s) is 50,000,000 and 60 bpm (one second a quarter) is 100,000,000.
#[test]
fn a_tempo_is_ten_ns_units_per_quarter() {
    assert_eq!(tempo(&set_tempo(50_000_000)), Bpm::new(120.0));
    assert_eq!(tempo(&set_tempo(100_000_000)), Bpm::new(60.0));
}

/// The slowest the field holds is 1.39698 bpm (section 7.5.3); a field of 0 is
/// "no valid tempo".
#[test]
fn the_field_has_a_slowest_tempo_and_zero_is_none() {
    let slowest = tempo(&set_tempo(u32::MAX)).unwrap().get();
    assert!((slowest - 1.39698).abs() < 1e-4, "{slowest}");
    assert_eq!(tempo(&set_tempo(0)), None);
    // The fastest reads back as 6e9 bpm.
    assert_eq!(tempo(&set_tempo(1)), Bpm::new(6.0e9));
}

#[test]
fn other_messages_are_not_a_tempo() {
    let clock = Message::System(MidiGroup::FIRST, tutti_ump::System::TimingClock);
    assert_eq!(tempo(&clock), None);
}
