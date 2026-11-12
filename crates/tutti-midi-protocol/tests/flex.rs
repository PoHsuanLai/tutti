//! Flex Set Tempo and Set Time Signature as unit values (M2-104 sections
//! 7.5.3 and 7.5.4).

use tutti_midi_protocol::{flex_tempo, flex_time_signature};
use tutti_types::{Bpm, TimeSignature};
use tutti_ump::TimeSignature as Wire;

fn wire(numerator: u8, denominator: u8) -> Wire {
    Wire {
        numerator,
        denominator,
        thirty_seconds: 8,
    }
}

/// Section 7.5.3: the field is 10 ns units per quarter note, so 120 bpm (0.5 s)
/// is 50,000,000 and 60 bpm (one second a quarter) is 100,000,000.
#[test]
fn a_tempo_is_ten_ns_units_per_quarter() {
    assert_eq!(flex_tempo(50_000_000), Bpm::new(120.0));
    assert_eq!(flex_tempo(100_000_000), Bpm::new(60.0));
}

/// The slowest the field holds is 1.39698 bpm (section 7.5.3); a field of 0 is
/// no tempo; the fastest, 1, is 6e9 bpm.
#[test]
fn the_field_has_a_slowest_tempo_and_zero_is_none() {
    let slowest = flex_tempo(u32::MAX).unwrap().get();
    assert!((slowest - 1.39698).abs() < 1e-4, "{slowest}");
    assert_eq!(flex_tempo(0), None);
    assert_eq!(flex_tempo(1), Bpm::new(6.0e9));
}

/// Section 7.5.4: the denominator is a negative power of 2, "2 represents a
/// quarter note, 3 represents an eighth note".
#[test]
fn the_denominator_is_a_power_of_two_exponent() {
    assert_eq!(
        flex_time_signature(wire(4, 2)),
        Some(TimeSignature::of(4, 4))
    );
    assert_eq!(
        flex_time_signature(wire(7, 3)),
        Some(TimeSignature::of(7, 8))
    );
    assert_eq!(
        flex_time_signature(wire(3, 1)),
        Some(TimeSignature::of(3, 2))
    );
    assert_eq!(
        flex_time_signature(wire(6, 6)),
        Some(TimeSignature::of(6, 64))
    );
}

/// What the meter cannot hold is refused, never clamped: a zero denominator is
/// "non-standard" (7.5.4), a numerator of 0 is not "1 to 256", and the meter's
/// own ceilings (64 beats, a 64th note) apply.
#[test]
fn what_the_meter_cannot_hold_is_refused() {
    for (n, d) in [(4, 0), (0, 2), (65, 2), (4, 7), (4, 31), (4, 32), (4, 255)] {
        assert_eq!(flex_time_signature(wire(n, d)), None, "{n}/{d}");
    }
    assert_eq!(
        flex_time_signature(wire(64, 2)),
        Some(TimeSignature::of(64, 4))
    );
}
