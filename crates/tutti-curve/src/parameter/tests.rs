//! The range contract for the two parameters. Neither has serde (nothing
//! persists a curve), so only `check_range_contract` runs.

use tutti_fixtures::{RangeContract, check_range_contract};

use super::*;

const NAN: f32 = f32::NAN;
const INF: f32 = f32::INFINITY;

#[test]
fn an_exponent_takes_exactly_the_finite_positive_floats() {
    check_range_contract(&RangeContract {
        new: Exponent::new,
        get: Exponent::get,
        of: Some(Exponent::of),
        valid: &[f32::MIN_POSITIVE, 0.5, 1.0, 2.0, 1.7, 1e30],
        invalid: &[0.0, -0.0, -2.0, f32::MIN, NAN, INF, -INF],
    });
    assert_eq!(
        format!("{:?}", Exponent::try_from(-2.0).unwrap_err()),
        "ExponentError(-2.0)"
    );
}

#[test]
fn a_curvature_takes_exactly_the_finite_floats_within_64() {
    check_range_contract(&RangeContract {
        new: Curvature::new,
        get: Curvature::get,
        of: Some(Curvature::of),
        valid: &[0.0, -0.0, 1e-5, 3.0, -3.0, 64.0, -64.0],
        invalid: &[64.001, -64.001, 100.0, NAN, INF, -INF, f32::MAX],
    });
    assert_eq!(
        format!("{:?}", Curvature::try_from(100.0).unwrap_err()),
        "CurvatureError(100.0)"
    );
}
