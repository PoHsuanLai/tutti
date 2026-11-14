//! `SampleRate` keeps the range-checked contract (`CLAUDE.md`, "Range-checked values").
#![cfg(feature = "serde")]

use tutti_fixtures::{RangeContract, check_range_contract, check_range_serde};
use tutti_types::{SampleRate, SampleRateOutOfRange};

/// A rate is finite and above zero, with no audible floor or ceiling: that is
/// the whole rule, and every consumer that wants a narrower one states it.
#[test]
fn a_rate_is_finite_and_above_zero() {
    let c = RangeContract {
        new: SampleRate::new,
        get: SampleRate::get,
        of: Some(SampleRate::of),
        valid: &[
            f64::MIN_POSITIVE,
            1.0,
            8_000.0,
            44_100.0,
            48_000.0,
            192_000.0,
            f64::MAX,
        ],
        invalid: &[
            0.0,
            -0.0,
            -48_000.0,
            f64::NEG_INFINITY,
            f64::INFINITY,
            f64::NAN,
        ],
    };
    check_range_contract(&c);
    check_range_serde(&c);
    assert_eq!(SampleRate::try_from(0.0), Err(SampleRateOutOfRange(0.0)));
    assert!(SampleRateOutOfRange(-5.0).to_string().contains("-5"));
}

/// A whole number of Hertz is a rate unless it is zero.
#[test]
fn whole_hertz_refuse_only_zero() {
    assert_eq!(SampleRate::from_hz(48_000), Some(SampleRate::SR_48K));
    assert_eq!(SampleRate::from_hz(0), None);
    assert_eq!(
        SampleRate::from_hz(u32::MAX).map(SampleRate::get),
        Some(f64::from(u32::MAX))
    );
}
