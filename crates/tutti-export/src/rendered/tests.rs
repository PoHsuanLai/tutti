//! `Rendered::new`'s refusals and the getters. That the type is sealed is the
//! `compile_fail` doctests on [`Rendered`](crate::Rendered).

use crate::{Rendered, RenderedError};
use tutti_types::{SampleRate, Samples};

const RATE: SampleRate = SampleRate(48_000.0);

/// No planes is refused.
#[test]
fn no_planes_is_refused() {
    assert_eq!(Rendered::new(vec![], RATE, 0), Err(RenderedError::NoPlanes));
}

/// Planes of unequal length are refused, naming the first that differs.
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
#[test]
fn an_invalid_rate_is_refused() {
    for rate in [f64::NAN, f64::INFINITY, 0.0, -48_000.0] {
        let err = Rendered::new(vec![vec![0.0; 4]], SampleRate(rate), 0).unwrap_err();
        assert!(matches!(err, RenderedError::InvalidRate { .. }), "{rate}");
    }
}

/// The getters read back what was given.
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

/// More planes than a layout holds are refused, so a `Rendered` always fits a
/// `Wave` (and its own `layout`).
#[test]
fn more_planes_than_a_layout_holds_are_refused() {
    let planes = vec![Vec::new(); usize::from(u16::MAX) + 1];
    assert_eq!(
        Rendered::new(planes, RATE, 0),
        Err(RenderedError::TooWide {
            planes: usize::from(u16::MAX) + 1
        })
    );
}

/// A render becomes the wave of its planes: its channels, length, rate and
/// samples, with the planes' order as the channels'.
#[test]
fn a_render_becomes_the_wave_of_its_planes() {
    let rendered =
        Rendered::new(vec![vec![0.1, 0.2, 0.3], vec![-0.1, -0.2, -0.3]], RATE, 7).unwrap();
    let wave = tutti_codec::Wave::from(rendered);
    assert_eq!(wave.channels(), 2);
    assert_eq!(wave.len(), 3);
    assert_eq!(wave.sample_rate(), RATE);
    assert_eq!(wave.channel(0), [0.1, 0.2, 0.3]);
    assert_eq!(wave.channel(1), [-0.1, -0.2, -0.3]);
}
