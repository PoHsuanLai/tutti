//! Mutation record (each planted, seen to fail, restored):
//! - `TEN_NS_UNITS_PER_MINUTE` off by a factor of 10 ->
//!   `a_tempo_is_ten_ns_units_per_quarter` and `the_field_saturates_and_is_never_zero`;
//! - `tempo_field` without the `.max(1)` -> `the_field_saturates_and_is_never_zero`;
//! - (the `field != 0` guard of `bpm_of` is not covered: `Bpm::new` refuses the
//!   infinity it would divide into, so the guard states the rule and removing it
//!   changes nothing.)
//! - `set_time_signature` writing 4 thirty-seconds -> `a_time_signature_is_a_flex_message`.

use super::*;
use tutti_ump::MidiGroup;

fn group(n: u8) -> MidiGroup {
    MidiGroup::new(n).unwrap()
}

/// M2-104 section 7.5.3: the field is 10 ns units per quarter note, so 120 bpm
/// (0.5 s) is 50,000,000, and the words are Figure 70's (type 0xD, group 6,
/// Format 0, Address 1, Status 0x00).
#[test]
fn a_tempo_is_ten_ns_units_per_quarter() {
    let ump = set_tempo(group(6), Bpm::of(120.0));
    assert_eq!(ump, Ump([0xD610_0000, 0x02FA_F080, 0, 0]));
    assert_eq!(tempo(&ump), Bpm::new(120.0));
    // 60 bpm: one second a quarter.
    let slow = set_tempo(group(0), Bpm::of(60.0));
    assert_eq!(slow.0[1], 100_000_000);
}

#[test]
fn the_field_saturates_and_is_never_zero() {
    // A tempo above 6e9 bpm would be a field of 0, "no valid tempo": it is 1.
    assert_eq!(tempo_field(Bpm::of(1.0e12)), 1);
    // The slowest the field holds is 1.39698 bpm (section 7.5.3).
    assert_eq!(tempo_field(Bpm::of(0.5)), u32::MAX);
    let slowest = bpm_of(u32::MAX).unwrap().get();
    assert!((slowest - 1.39698).abs() < 1e-4, "{slowest}");
    assert_eq!(bpm_of(0), None);
    assert_eq!(tempo_field(Bpm::of(f64::MAX)), 1);
    assert_eq!(tempo_field(Bpm::of(f64::MIN_POSITIVE)), u32::MAX);
    // The fastest tempo reads back as 6e9 bpm.
    assert_eq!(
        tempo(&set_tempo(group(0), Bpm::of(1.0e10))),
        Bpm::new(6.0e9)
    );
}

/// The field is integer-quantized, so a tempo comes back to within a hair.
#[test]
fn a_tempo_round_trips() {
    for bpm in [60.0, 120.0, 140.0, 174.0] {
        let back = tempo(&set_tempo(group(0), Bpm::of(bpm))).expect("is a Set Tempo");
        assert!((back.get() - bpm).abs() < 0.05, "bpm {bpm} -> {back}");
    }
}

/// M2-104 section 7.5.4: numerator, denominator (a negative power of 2) and
/// the 32nd notes in 24 clocks, after the status word of Figure 71.
#[test]
fn a_time_signature_is_a_flex_message() {
    let ump = set_time_signature(group(6), 3, 2);
    assert_eq!(ump, Ump([0xD610_0001, 0x0302_0800, 0, 0]));
    assert_eq!(time_signature(&ump), Some((3, 2)));
}

#[test]
fn other_packets_are_neither() {
    let clock: Ump = Message::System(group(0), tutti_ump::System::TimingClock).into();
    assert_eq!(tempo(&clock), None);
    assert_eq!(time_signature(&clock), None);
    // A tempo is not a time signature and the reverse.
    assert_eq!(time_signature(&set_tempo(group(0), Bpm::of(90.0))), None);
    assert_eq!(tempo(&set_time_signature(group(0), 4, 2)), None);
    // A Set Tempo with the "no valid tempo" field reads as no tempo.
    let none: Ump = Message::Flex(group(0), Flex::SetTempo(0)).into();
    assert_eq!(tempo(&none), None);
}
