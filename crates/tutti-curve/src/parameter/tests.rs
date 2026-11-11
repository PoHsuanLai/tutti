//! The range contract of `CLAUDE.md` ("Range-checked values") for the two
//! parameters, written out by hand: `tutti_fixtures::check_range_contract`
//! needs serde on the type, and `tutti-curve` has none (nothing persists a
//! curve).

use super::*;

const NAN: f32 = f32::NAN;
const INF: f32 = f32::INFINITY;

#[test]
fn an_exponent_takes_exactly_the_finite_positive_floats() {
    for p in [f32::MIN_POSITIVE, 0.5, 1.0, 2.0, 1.7, 1e30] {
        let e = Exponent::new(p).expect("valid");
        assert_eq!(e.get(), p);
        assert_eq!(Exponent::try_from(p), Ok(e));
        assert_eq!(f32::from(e), p);
        assert_eq!(Exponent::of(p), e);
    }
    for p in [0.0, -0.0, -2.0, f32::MIN, NAN, INF, -INF] {
        assert!(Exponent::new(p).is_none(), "new accepted {p:?}");
        let err = Exponent::try_from(p).expect_err("refused");
        assert_eq!(format!("{err:?}"), format!("ExponentError({p:?})"));
        assert!(
            std::panic::catch_unwind(|| Exponent::of(p)).is_err(),
            "of accepted {p:?}"
        );
    }
}

#[test]
fn a_curvature_takes_exactly_the_finite_floats_within_64() {
    for k in [0.0, -0.0, 1e-5, 3.0, -3.0, 64.0, -64.0] {
        let c = Curvature::new(k).expect("valid");
        assert_eq!(c.get(), k);
        assert_eq!(Curvature::try_from(k), Ok(c));
        assert_eq!(f32::from(c), k);
        assert_eq!(Curvature::of(k), c);
    }
    for k in [64.001, -64.001, 100.0, NAN, INF, -INF, f32::MAX] {
        assert!(Curvature::new(k).is_none(), "new accepted {k:?}");
        let err = Curvature::try_from(k).expect_err("refused");
        assert_eq!(format!("{err:?}"), format!("CurvatureError({k:?})"));
        assert!(
            std::panic::catch_unwind(|| Curvature::of(k)).is_err(),
            "of accepted {k:?}"
        );
    }
}
