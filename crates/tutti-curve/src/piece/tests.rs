use super::*;
use crate::Exponent;

/// `value` is `from + (to - from) * g(t)`, in either direction, and a hold
/// keeps `from` until the end.
#[test]
fn value_scales_the_law_onto_the_endpoints() {
    assert_eq!(Piece::new(2.0, 6.0, Law::Linear).value(0.25), 3.0);
    assert_eq!(
        Piece::new(2.0, 6.0, Law::PowerIn(Exponent::of(2.0))).value(0.5),
        3.0
    );
    assert_eq!(
        Piece::new(6.0, 2.0, Law::PowerIn(Exponent::of(2.0))).value(0.5),
        5.0
    );
    assert_eq!(Piece::new(2.0, 6.0, Law::Hold).value(0.5), 2.0);
    assert_eq!(Piece::new(2.0, 6.0, Law::Hold).value(1.0), 6.0);
}
