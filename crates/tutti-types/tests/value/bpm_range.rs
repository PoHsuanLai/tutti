//! `Bpm` keeps the range-checked contract: `new` refuses, `TryFrom` and serde
//! agree with it.
#![cfg(feature = "serde")]

use tutti_fixtures::{RangeContract, check_range_contract, check_range_serde};
use tutti_types::{Bpm, BpmOutOfRange};

/// A tempo is finite and above zero, with no musical floor or ceiling.
#[test]
fn a_tempo_is_finite_and_above_zero() {
    let c = RangeContract {
        new: Bpm::new,
        get: Bpm::get,
        of: Some(Bpm::of),
        valid: &[f64::MIN_POSITIVE, 0.014, 1.0, 120.0, 999.0, 6.0e7, f64::MAX],
        invalid: &[
            0.0,
            -0.0,
            -120.0,
            f64::NEG_INFINITY,
            f64::INFINITY,
            f64::NAN,
        ],
    };
    check_range_contract(&c);
    check_range_serde(&c);
    assert_eq!(Bpm::try_from(0.0), Err(BpmOutOfRange(0.0)));
    assert!(BpmOutOfRange(-5.0).to_string().contains("-5"));
}

/// The default is in range.
#[test]
fn the_default_is_120() {
    assert_eq!(Bpm::default(), Bpm::of(120.0));
}
