use super::*;
use tutti_fixtures::midi;
use tutti_ump::{MidiChannel, TimeSignature as Wire};

fn tempo_in(group: u8, field: u32) -> Ump {
    Ump::from(Message::Flex(MidiGroup::of(group), Flex::SetTempo(field)))
}

fn meter_in(group: u8, numerator: u8, denominator: u8) -> Ump {
    Ump::from(Message::Flex(
        MidiGroup::of(group),
        Flex::SetTimeSignature(Wire {
            numerator,
            denominator,
            thirty_seconds: 8,
        }),
    ))
}

fn follower(group: u8, meter: bool) -> TempoFollower {
    TempoFollower::new(TempoSource::Flex(MidiGroup::of(group)), meter)
}

/// **A Set Tempo of the followed Group is its tempo**: the field is 10 ns per
/// quarter note (M2-104 §7.5.3), so 0.5 s is 120 bpm and 1 s is 60 bpm.
/// Spec: m2-104/msg/d/00.00
#[test]
fn a_set_tempo_of_the_followed_group_is_its_tempo() {
    let f = follower(2, false);
    assert_eq!(
        f.step(tempo_in(2, 50_000_000)),
        Some(Followed::Tempo(Bpm::of(120.0)))
    );
    assert_eq!(
        f.step(tempo_in(2, 100_000_000)),
        Some(Followed::Tempo(Bpm::of(60.0)))
    );
}

/// **A Group is a cable of its own** (§2.1.2, Appendix B.1): a Set Tempo or Set
/// Time Signature in another Group is not the followed source's.
#[test]
fn another_group_is_ignored() {
    let f = follower(2, true);
    assert_eq!(f.step(tempo_in(0, 50_000_000)), None);
    assert_eq!(f.step(tempo_in(3, 50_000_000)), None);
    assert_eq!(f.step(meter_in(1, 3, 2)), None);
}

/// **What the unit values refuse is not followed**: a tempo field of `0` (no
/// valid tempo), a numerator of `0` or above `BeatsPerBar`'s 64, a denominator
/// field of `0` (non-standard) and one past `NoteValue`'s 64th note.
#[test]
fn what_the_unit_values_refuse_is_not_followed() {
    let f = follower(0, true);
    assert_eq!(f.step(tempo_in(0, 0)), None);
    assert_eq!(f.step(meter_in(0, 0, 2)), None, "no beats in a bar");
    assert_eq!(f.step(meter_in(0, 65, 2)), None, "more than 64 beats");
    assert_eq!(f.step(meter_in(0, 4, 0)), None, "non-standard denominator");
    assert_eq!(f.step(meter_in(0, 4, 7)), None, "shorter than a 64th note");
    // The neighbours of each refusal are followed.
    assert!(f.step(tempo_in(0, 1)).is_some());
    assert!(f.step(meter_in(0, 64, 6)).is_some());
}

/// **The meter is followed only when asked**: Set Time Signature gives
/// 7 beats of an eighth note (numerator 7, denominator field 3).
#[test]
fn the_meter_is_followed_only_when_asked() {
    assert_eq!(
        follower(0, true).step(meter_in(0, 7, 3)),
        Some(Followed::Meter(TimeSignature::of(7, 8)))
    );
    assert_eq!(follower(0, false).step(meter_in(0, 7, 3)), None);
    // The tempo is followed either way.
    assert!(follower(0, false).step(tempo_in(0, 50_000_000)).is_some());
}

/// **A Set Tempo is a Group message** (§7.5.3: Address 1): the same packet
/// addressed to a Channel is no Set Tempo, and is ignored.
#[test]
fn a_set_tempo_addressed_to_a_channel_is_ignored() {
    let mut ump = tempo_in(0, 50_000_000);
    assert!(
        follower(0, true).step(ump).is_some(),
        "as a Group message it is followed"
    );
    // Address (bits 21..20 of the first word): 1 is the Group, 0 a Channel.
    ump.0[0] = (ump.0[0] & !(0x3 << 20)) | (u32::from(MidiChannel::of(5).get()) << 16);
    assert_eq!(follower(0, true).step(ump), None);
}

/// Other messages are no tempo: another Flex Data message and a note.
#[test]
fn other_messages_are_not_followed() {
    let f = follower(0, true);
    let metronome = Ump::from(Message::Flex(
        MidiGroup::FIRST,
        Flex::SetMetronome(tutti_ump::Metronome {
            clocks_per_click: 24,
            accent_1: 0,
            accent_2: 0,
            accent_3: 0,
            subdivision_1: 0,
            subdivision_2: 0,
        }),
    ));
    assert_eq!(f.step(metronome), None);
    assert_eq!(f.step(midi::note_on(MidiChannel::FIRST, 60, 0x8000)), None);
}
