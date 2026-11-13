//! `BeatsPerCycle` keeps the range-checked contract (`CLAUDE.md`, "Range-checked values").
#![cfg(feature = "serde")]

use tutti_fixtures::{RangeContract, check_range_contract, check_range_serde};
use tutti_types::{Beat, BeatsPerCycle, BeatsPerCycleOutOfRange};

/// A cycle's span is finite and above zero beats, so a beat divided by it is
/// always a cycle position.
#[test]
fn a_span_is_finite_and_above_zero() {
    let c = RangeContract {
        new: BeatsPerCycle::new,
        get: BeatsPerCycle::get,
        of: Some(BeatsPerCycle::of),
        valid: &[f64::MIN_POSITIVE, 0.25, 1.0, 4.0, 64.0, f64::MAX],
        invalid: &[0.0, -0.0, -4.0, f64::NEG_INFINITY, f64::INFINITY, f64::NAN],
    };
    check_range_contract(&c);
    check_range_serde(&c);
    assert_eq!(
        BeatsPerCycle::try_from(0.0),
        Err(BeatsPerCycleOutOfRange(0.0))
    );
    assert!(BeatsPerCycleOutOfRange(-2.0).to_string().contains("-2"));
}

/// A beat on a clock of 4 beats a cycle is a quarter cycle a beat.
#[test]
fn a_beat_is_its_share_of_the_span() {
    assert_eq!(Beat(1.0).cycles_in(BeatsPerCycle::of(4.0)), 0.25);
    assert_eq!(Beat(10.0).cycles_in(BeatsPerCycle::of(4.0)), 2.5);
}
