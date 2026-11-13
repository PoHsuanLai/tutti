//! [`Routing`]: which channel's delayed tap recirculates into which channel's line.

use tutti_types::BusTopology;

/// The widest routing: a bus's channel count.
pub(crate) const MAX_WIDTH: usize = tutti_types::MAX_BUS_CHANNELS;

/// The cross-feedback routing of a delay: entry `(c, j)` is how much of channel `j`'s
/// delayed tap recirculates into channel `c`'s line, before the cross-feedback amount
/// scales it.
///
/// A range-checked value: square on one bus, every
/// weight finite and **every row's absolute sum at most 1**. With the self and cross
/// feedback bounded as a pair ([`Feedback::stable_pair`](tutti_types::Feedback::stable_pair)),
/// that keeps each line's loop gain under
/// [`Feedback::MAX_STABLE`](tutti_types::Feedback::MAX_STABLE) whatever the matrix says.
/// [`new`](Self::new) refuses a row that sums past 1; [`normalized`](Self::normalized) is
/// the named saturation, which scales it down. A weight that is not finite is refused by
/// both: it would survive the scaling (`inf / inf` is NaN) and poison the loop it feeds
/// for good.
///
/// The presets: [`none`](Self::none) (no recirculation between channels: they render
/// independently), [`swap`](Self::swap) (a stereo ping-pong, `[[0, 1], [1, 0]]`) and
/// [`ring`](Self::ring) (channel `c` fed by `c - 1`, which spins echoes around a surround
/// field).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Routing {
    input: BusTopology,
    weights: [[f32; MAX_WIDTH]; MAX_WIDTH],
}

/// A refused routing.
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum RoutingError {
    /// The matrix is not `width x width`, row-major.
    #[error("a {width}-channel routing has {expected} weights, not {got}")]
    Length {
        /// The bus's width.
        width: usize,
        /// `width * width`.
        expected: usize,
        /// What was given.
        got: usize,
    },
    /// A weight is not finite.
    #[error("the weight at row {row}, column {column} is {weight}, not finite")]
    NotFinite {
        /// The row, the channel the tap feeds.
        row: usize,
        /// The column, the channel the tap is of.
        column: usize,
        /// The refused weight.
        weight: f32,
    },
    /// A row's absolute weights sum past 1 ([`Routing::normalized`] scales it down).
    #[error("row {row} sums to {sum}, past 1")]
    Row {
        /// The row.
        row: usize,
        /// The sum of its absolute weights.
        sum: f32,
    },
    /// The routing is for another bus than the delay's.
    #[error("a routing for {routing} channels on a {family}-channel delay")]
    Width {
        /// The routing's width.
        routing: usize,
        /// The delay's.
        family: usize,
    },
}

impl Routing {
    /// No routing between channels: each renders independently, and the cross-feedback
    /// is inert.
    #[must_use]
    pub const fn none(input: BusTopology) -> Self {
        Self {
            input,
            weights: [[0.0; MAX_WIDTH]; MAX_WIDTH],
        }
    }

    /// The stereo ping-pong: each channel is fed by the other, `[[0, 1], [1, 0]]`.
    #[must_use]
    pub const fn swap() -> Self {
        let mut r = Self::none(BusTopology::STEREO);
        r.weights[0][1] = 1.0;
        r.weights[1][0] = 1.0;
        r
    }

    /// A ring: channel `c` is fed by channel `c - 1` (the first by the last), so an echo
    /// goes round the field. A mono bus has no other channel to feed from: it is
    /// [`none`](Self::none).
    #[must_use]
    pub fn ring(input: BusTopology) -> Self {
        let mut r = Self::none(input);
        let n = r.width();
        if n > 1 {
            for c in 0..n {
                r.weights[c][(c + n - 1) % n] = 1.0;
            }
        }
        r
    }

    /// The routing `matrix`, `width x width` row-major: entry `matrix[c * width + j]`
    /// is how much of channel `j`'s tap feeds channel `c`. Refuses a matrix of another
    /// length, a weight that is not finite and a row whose absolute weights sum past 1.
    ///
    /// # Errors
    ///
    /// [`RoutingError::Length`], [`NotFinite`](RoutingError::NotFinite) or
    /// [`Row`](RoutingError::Row), the first found.
    pub fn new(input: BusTopology, matrix: &[f32]) -> Result<Self, RoutingError> {
        let mut r = Self::checked(input, matrix)?;
        let n = r.width();
        for (c, row) in matrix.chunks(n).enumerate() {
            let sum: f32 = row.iter().map(|w| w.abs()).sum();
            if sum > 1.0 {
                return Err(RoutingError::Row { row: c, sum });
            }
            r.weights[c][..n].copy_from_slice(row);
        }
        Ok(r)
    }

    /// As [`new`](Self::new), with each row that sums past 1 scaled down to sum to 1
    /// (to within `f32` rounding) instead of refused. The named saturation: the one way
    /// a matrix is rescaled.
    ///
    /// # Errors
    ///
    /// [`RoutingError::Length`] or [`NotFinite`](RoutingError::NotFinite).
    pub fn normalized(input: BusTopology, matrix: &[f32]) -> Result<Self, RoutingError> {
        let mut r = Self::checked(input, matrix)?;
        let n = r.width();
        for (c, row) in matrix.chunks(n).enumerate() {
            let sum: f32 = row.iter().map(|w| w.abs()).sum();
            let scale = if sum > 1.0 { 1.0 / sum } else { 1.0 };
            for (j, &w) in row.iter().enumerate() {
                r.weights[c][j] = w * scale;
            }
        }
        Ok(r)
    }

    /// `none(input)` after the length and finiteness of `matrix` are checked.
    fn checked(input: BusTopology, matrix: &[f32]) -> Result<Self, RoutingError> {
        let r = Self::none(input);
        let width = r.width();
        if matrix.len() != width * width {
            return Err(RoutingError::Length {
                width,
                expected: width * width,
                got: matrix.len(),
            });
        }
        if let Some(i) = matrix.iter().position(|w| !w.is_finite()) {
            return Err(RoutingError::NotFinite {
                row: i / width,
                column: i % width,
                weight: matrix[i],
            });
        }
        Ok(r)
    }

    /// The bus this routing is for.
    #[must_use]
    pub const fn input(&self) -> BusTopology {
        self.input
    }

    /// The channels it routes.
    #[must_use]
    pub const fn width(&self) -> usize {
        self.input.layout().count() as usize
    }

    /// How much of channel `from`'s tap feeds channel `to`'s line: 0 for a channel
    /// that is not on the bus.
    #[must_use]
    pub fn weight(&self, to: usize, from: usize) -> f32 {
        if to < MAX_WIDTH && from < MAX_WIDTH {
            self.weights[to][from]
        } else {
            0.0
        }
    }

    /// Whether channel `to` is fed by no tap.
    #[must_use]
    pub fn is_none(&self) -> bool {
        self.weights.iter().flatten().all(|&w| w == 0.0)
    }

    /// The sum of the absolute weights of row `to`: at most 1.
    #[must_use]
    pub fn row_sum(&self, to: usize) -> f32 {
        self.weights
            .get(to)
            .map_or(0.0, |row| row[..self.width()].iter().map(|w| w.abs()).sum())
    }
}
