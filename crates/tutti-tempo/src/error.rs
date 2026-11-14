/// Why a [`TempoMap`](crate::TempoMap) was refused. An index is the position of
/// the offending point in the list given.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TempoError {
    /// No points.
    #[error("a tempo map needs at least one point")]
    Empty,
    /// The first point is not at beat 0.
    #[error("the first tempo point must be at beat 0")]
    FirstNotAtZero,
    /// A beat that is not finite, or not after the previous point's.
    #[error("tempo point {0} is not at a finite beat after the previous point")]
    BadOrder(usize),
    /// The last point is a ramp, which has no next tempo to move to.
    #[error("the last tempo point cannot ramp: it has no next tempo")]
    RampWithoutEnd,
    /// A segment whose duration cannot be held in an `f64` (a tempo or a span
    /// so extreme that its square or its quotient overflows).
    #[error("the segment from tempo point {0} is not representable")]
    Unrepresentable(usize),
}
