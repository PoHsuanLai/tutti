use std::fmt;
use tutti_types::{Beat, Bpm, Frame, SampleRate};

/// The length in frames of a [`Cell`]: how often a ramp's tempo is re-seated.
///
/// Never zero. [`new`](Self::new) refuses 0; [`of`](Self::of) is for a literal.
/// A caller picks one value and keeps it, so a ramp lands on the same frames at
/// any buffer size.
///
/// It is a `u64` of frames, not `Samples`: a position and a grid are `Frame`
/// arithmetic here (`Samples` is the `usize` count at the I/O boundary), and
/// `Samples` cannot say "never zero".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "u64", into = "u64"))]
pub struct CellGrid(u64);

/// A [`CellGrid`] of zero frames was asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellGridError(pub u64);

impl fmt::Display for CellGridError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a cell grid is at least one frame, not {}", self.0)
    }
}

impl std::error::Error for CellGridError {}

impl CellGrid {
    /// The grid of `frames` frames, or `None` for 0.
    #[must_use]
    pub const fn new(frames: u64) -> Option<Self> {
        if frames == 0 {
            None
        } else {
            Some(Self(frames))
        }
    }

    /// The grid for a literal or a constant. For a value that arrives from
    /// outside, use [`new`](Self::new).
    ///
    /// # Panics
    ///
    /// When `frames` is 0; in a `const` that is a compile error.
    #[must_use]
    #[track_caller]
    pub const fn of(frames: u64) -> Self {
        match Self::new(frames) {
            Some(g) => g,
            None => panic!("a cell grid is at least one frame"),
        }
    }

    /// The length in frames.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl TryFrom<u64> for CellGrid {
    type Error = CellGridError;

    fn try_from(frames: u64) -> Result<Self, CellGridError> {
        Self::new(frames).ok_or(CellGridError(frames))
    }
}

impl From<CellGrid> for u64 {
    fn from(g: CellGrid) -> u64 {
        g.0
    }
}

/// A constant-tempo stretch of frames `start..end` inside one segment of a map.
///
/// In a step segment the tempo is the segment's own, to the bit, so a reader
/// that restarts when the tempo changes restarts only at a real change. In a
/// ramp it is the chord between the map's beats on `start` and `end`, which
/// differs from cell to cell: that is the ramp, not noise, and a reader that
/// restarts on a tempo change restarts once per cell for as long as the ramp
/// lasts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cell {
    /// First frame of the cell.
    pub start: Frame,
    /// First frame of the next cell.
    pub end: Frame,
    /// The map's beat on `start`.
    pub beat: Beat,
    /// The cell's constant tempo.
    pub tempo: Bpm,
}

impl Cell {
    /// The beat on `frame` by the cell's constant tempo. Meant for `frame` in
    /// `start..=end`; outside it the line continues.
    #[must_use]
    pub fn beat_at(&self, frame: Frame, rate: SampleRate) -> Beat {
        let n = frame.get() as f64 - self.start.get() as f64;
        Beat(self.beat.get() + n * self.tempo.get() / (60.0 * rate.get()))
    }
}
