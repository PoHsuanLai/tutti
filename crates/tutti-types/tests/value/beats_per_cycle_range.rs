//! `BeatsPerCycle` keeps the range-checked contract (`CLAUDE.md`, "Range-checked values").
#![cfg(feature = "serde")]

use tutti_fixtures::{RangeContract, check_range_contract};
use tutti_types::{BeatsPerCycle, BeatsPerCycleOutOfRange};

/// A cycle's span is finite and above zero beats, so a beat divided by it is
/// always a cycle position.
#[test]
fn a_span_is_finite_and_above_zero() {
    check_range_contract(&RangeContract {
        new: BeatsPerCycle::new,
        get: BeatsPerCycle::get,
        of: Some(BeatsPerCycle::of),
        valid: &[f64::MIN_POSITIVE, 0.25, 1.0, 4.0, 64.0, f64::MAX],
        invalid: &[0.0, -0.0, -4.0, f64::NEG_INFINITY, f64::INFINITY, f64::NAN],
    });
    assert_eq!(
        BeatsPerCycle::try_from(0.0),
        Err(BeatsPerCycleOutOfRange(0.0))
    );
    assert!(BeatsPerCycleOutOfRange(-2.0).to_string().contains("-2"));
}
