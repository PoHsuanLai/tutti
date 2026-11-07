use std::fmt;

/// Why a [`TempoMap`](crate::TempoMap) was refused. An index is the position of
/// the offending point in the list given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TempoError {
    /// No points.
    Empty,
    /// The first point is not at beat 0.
    FirstNotAtZero,
    /// A tempo that is not finite and positive.
    BadTempo(usize),
    /// A beat that is not finite, or not after the previous point's.
    BadOrder(usize),
    /// The last point is a ramp, which has no next tempo to move to.
    RampWithoutEnd,
    /// A segment whose duration cannot be held in an `f64` (a tempo or a span
    /// so extreme that its square or its quotient overflows).
    Unrepresentable(usize),
}

impl fmt::Display for TempoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("a tempo map needs at least one point"),
            Self::FirstNotAtZero => f.write_str("the first tempo point must be at beat 0"),
            Self::BadTempo(i) => write!(f, "tempo point {i} is not a finite, positive tempo"),
            Self::BadOrder(i) => write!(
                f,
                "tempo point {i} is not at a finite beat after the previous point"
            ),
            Self::RampWithoutEnd => {
                f.write_str("the last tempo point cannot ramp: it has no next tempo")
            }
            Self::Unrepresentable(i) => {
                write!(f, "the segment from tempo point {i} is not representable")
            }
        }
    }
}

impl std::error::Error for TempoError {}
