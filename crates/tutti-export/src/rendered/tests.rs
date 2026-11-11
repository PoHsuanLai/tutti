//! `Rendered::new`'s refusals and the getters. That the type is sealed is the
//! `compile_fail` doctests on [`Rendered`](crate::Rendered).

use crate::{Rendered, RenderedError};
use tutti_types::{SampleRate, Samples};

const RATE: SampleRate = SampleRate(48_000.0);

/// No planes is refused.
///
/// Mutation (run): drop the `first()` guard (treat none as fine) -> `Ok` ->
/// fails.
#[test]
fn no_planes_is_refused() {
    assert_eq!(Rendered::new(vec![], RATE, 0), Err(RenderedError::NoPlanes));
}

/// Planes of unequal length are refused, naming the first that differs.
///
/// Mutation (run): drop the length comparison -> `Ok` -> fails; compare
/// against the last plane instead of the first -> `plane` reads 1, not 2 ->
/// fails.
#[test]
fn unequal_planes_are_refused() {
    let err = Rendered::new(vec![vec![0.0; 4], vec![0.0; 4], vec![0.0; 5]], RATE, 0).unwrap_err();
    assert_eq!(
        err,
        RenderedError::UnequalPlanes {
            plane: 2,
            expected: Samples(4),
            found: Samples(5),
        }
    );
}

/// A rate that is not finite and positive is refused.
///
/// Mutation (run): drop the `is_rate` check -> `Ok` -> fails.
#[test]
fn an_invalid_rate_is_refused() {
    for rate in [f64::NAN, f64::INFINITY, 0.0, -48_000.0] {
        let err = Rendered::new(vec![vec![0.0; 4]], SampleRate(rate), 0).unwrap_err();
        assert!(matches!(err, RenderedError::InvalidRate { .. }), "{rate}");
    }
}

/// The getters read back what was given.
///
/// Mutation (run): `dropped_events()` returning 0 -> fails;
/// `into_planes` returning the planes reversed -> fails.
#[test]
fn the_getters_read_back_what_was_given() {
    let planes = vec![vec![1.0, 2.0, 3.0], vec![4.0, 5.0, 6.0]];
    let rendered = Rendered::new(planes.clone(), RATE, 7).unwrap();
    assert_eq!(rendered.planes(), planes.as_slice());
    assert_eq!(rendered.channels(), 2);
    assert_eq!(rendered.frames(), Samples(3));
    assert_eq!(rendered.sample_rate(), RATE);
    assert_eq!(rendered.dropped_events(), 7);
    assert_eq!(rendered.into_planes(), planes);
}
