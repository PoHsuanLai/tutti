//! The point list against what a breakpoint curve is: the figures are written
//! out from each law's closed form.

use tutti_types::Beat;

use super::{AutomationEnvelope, AutomationPoint};
use crate::Law;

fn point(beat: f64, value: f32) -> AutomationPoint {
    AutomationPoint::new(Beat(beat), value)
}

fn ramp() -> AutomationEnvelope {
    let mut e = AutomationEnvelope::new();
    e.add_point(AutomationPoint::with_curve(
        Beat(1.0),
        0.0,
        Law::PowerIn(2.0),
    ));
    e.add_point(point(3.0, 1.0));
    e
}

/// No point, no value; one point is its value everywhere.
///
/// Mutation (run): an early `return Some(0.0)` for an empty list gives
/// `Some(0.0)` for the empty one.
#[test]
fn an_empty_envelope_has_no_value_and_one_point_is_everywhere() {
    assert_eq!(AutomationEnvelope::new().value_at(Beat(1.0)), None);
    let mut one = AutomationEnvelope::new();
    one.add_point(point(2.0, 0.7));
    for b in [-1.0, 2.0, 9.0] {
        assert_eq!(one.value_at(Beat(b)), Some(0.7));
    }
}

/// Before the first point and from the last on, the end values hold; between
/// them a piece follows its starting point's law: here `t²` over `[1, 3]`, so
/// beat 2 is `t = 1/2`, a quarter, and beat 2.5 is `t = 3/4`, 9/16.
///
/// Mutation (run): the piece taking the law of the point it ends at (`q`, a
/// linear one) gives 0.5 at beat 2 and fails; `>=` written `>` in the last
/// point's hold changes nothing observable (the closed form at `t = 1` is
/// the last value), so it is not a mutation of this test.
#[test]
fn a_piece_follows_its_starting_points_law() {
    let e = ramp();
    assert_eq!(e.value_at(Beat(0.0)), Some(0.0));
    assert_eq!(e.value_at(Beat(1.0)), Some(0.0));
    assert_eq!(e.value_at(Beat(2.0)), Some(0.25));
    assert_eq!(e.value_at(Beat(2.5)), Some(0.5625));
    assert_eq!(e.value_at(Beat(3.0)), Some(1.0));
    assert_eq!(e.value_at(Beat(9.0)), Some(1.0));
}

/// A breakpoint reads its own value, whatever the law before it, and each
/// piece starts at its point.
///
/// Mutation (run): the piece taking the law of the point it ends at fails the
/// first assertion (a `Hold` ends at 5 where its own start reads 0). Not a
/// mutation of this test: `partition_point` with `<` for `<=`, which reads the
/// breakpoint from the piece before it at `t = 1`, where every law gives `to`.
#[test]
fn a_breakpoint_reads_its_own_value() {
    let mut e = AutomationEnvelope::new();
    e.add_point(AutomationPoint::with_curve(Beat(0.0), 0.0, Law::Hold));
    e.add_point(AutomationPoint::with_curve(Beat(2.0), 5.0, Law::Hold));
    e.add_point(point(4.0, 9.0));
    assert_eq!(e.value_at(Beat(1.999)), Some(0.0));
    assert_eq!(e.value_at(Beat(2.0)), Some(5.0));
    assert_eq!(e.value_at(Beat(3.999)), Some(5.0));
}

/// Points sort by time as they come, and a point at an existing time replaces
/// it.
///
/// Mutation (run): `Ok(i) => insert` instead of replace leaves 4 points;
/// `Err(i) => push` leaves them unsorted.
#[test]
fn add_point_keeps_time_order_and_replaces_a_tie() {
    let mut e = AutomationEnvelope::new();
    e.add_point(point(2.0, 0.2))
        .add_point(point(0.0, 0.0))
        .add_point(point(1.0, 0.1));
    e.add_point(point(1.0, 0.9));
    let times: Vec<f64> = e.points().iter().map(|p| p.time.get()).collect();
    assert_eq!(times, [0.0, 1.0, 2.0]);
    assert_eq!(e.points()[1].value, 0.9);
    assert_eq!((e.len(), e.is_empty()), (3, false));
}

/// A range clamps every value read, a held end and a piece's middle alike.
///
/// Mutation (run): `clamped` skipped on the interpolated arm lets the 2.0 at
/// beat 1.5 through; `max` and `min` swapped clamps to the wrong ends.
#[test]
fn a_range_clamps_what_is_read() {
    let mut e = AutomationEnvelope::new().with_range(0.0, 1.0);
    e.add_point(point(0.0, -1.0));
    e.add_point(point(2.0, 3.0));
    assert_eq!(e.range(), Some((0.0, 1.0)));
    assert_eq!(e.value_at(Beat(0.0)), Some(0.0));
    assert_eq!(e.value_at(Beat(0.25)), Some(0.0));
    assert_eq!(e.value_at(Beat(1.0)), Some(1.0));
    assert_eq!(e.value_at(Beat(1.5)), Some(1.0));
    assert_eq!(e.value_at(Beat(2.0)), Some(1.0));
}

/// Simplify drops a point its neighbours' piece already gives and keeps a
/// corner; the ends always stay.
///
/// Mutation (run): the comparison `> tolerance` written `>= 0.0` keeps the
/// collinear point; `<` written for `>` drops the corner.
#[test]
fn simplify_drops_the_redundant_and_keeps_the_corner() {
    let mut e = AutomationEnvelope::new();
    for (b, v) in [(0.0, 0.0), (1.0, 0.25), (2.0, 0.5), (3.0, 1.0), (4.0, 0.0)] {
        e.add_point(point(b, v));
    }
    e.simplify(0.01);
    let kept: Vec<(f64, f32)> = e.points().iter().map(|p| (p.time.get(), p.value)).collect();
    assert_eq!(kept, [(0.0, 0.0), (2.0, 0.5), (3.0, 1.0), (4.0, 0.0)]);
}

/// The test of redundancy is the neighbour's own law: a point on a `t²`
/// piece is redundant, the same point on a line is not.
///
/// Mutation (run): `prev.curve` replaced by `Law::Linear` in `simplify` keeps
/// the first envelope's middle point.
#[test]
fn simplify_reads_the_previous_points_law() {
    let build = |law| {
        let mut e = AutomationEnvelope::new();
        e.add_point(AutomationPoint::with_curve(Beat(0.0), 0.0, law));
        e.add_point(point(1.0, 0.25));
        e.add_point(point(2.0, 1.0));
        e.simplify(0.01);
        e.len()
    };
    assert_eq!(build(Law::PowerIn(2.0)), 2);
    assert_eq!(build(Law::Linear), 3);
}
