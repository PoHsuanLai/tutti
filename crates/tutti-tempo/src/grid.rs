use crate::TempoMap;
use std::fmt;
use tutti_types::{Beat, Bpm, Frame, SampleRate};

/// The length in frames of a [`Cell`]: how often a ramp's tempo is re-seated.
///
/// Never zero. [`new`](Self::new) refuses 0; [`of`](Self::of) is for a literal.
/// A caller picks one value and keeps it, so a ramp lands on the same frames at
/// any buffer size.
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

/// A constant-tempo cell of a map: starting on an absolute grid frame, with the
/// map's exact beat there, moving at the chord tempo to the next grid frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cell {
    /// First frame of the cell (a multiple of the grid).
    pub start: Frame,
    /// First frame of the next cell.
    pub end: Frame,
    /// Exact beat on `start`.
    pub beat: Beat,
    /// The tempo that lands exactly on the map's beat on `end`.
    pub tempo: Bpm,
}

impl Cell {
    /// The beat on `frame`, by the cell's constant tempo. Meant for `frame` in
    /// `start..=end`; outside it the line continues.
    #[must_use]
    pub fn beat_at(&self, frame: Frame, rate: SampleRate) -> Beat {
        let n = frame.get() as f64 - self.start.get() as f64;
        Beat(self.beat.get() + n * self.tempo.get() / (60.0 * rate.get()))
    }
}

/// The cell holding `frame`, on a grid of `grid` frames.
///
/// A pure function of `(map, frame, grid, rate)`: where a caller's blocks begin
/// and end cannot change it, so the same map gives identical cells at identical
/// absolute frames at any buffer size.
///
/// Inside a ramp the chord differs from the curve by a fraction of a frame that
/// shrinks with the square of `grid`. A cell that contains a step point averages
/// the two tempos; a caller cuts at steps (they are exact in the map) and takes
/// cells for the ramps.
#[must_use]
pub fn chord_cell(map: &TempoMap, frame: Frame, grid: CellGrid, rate: SampleRate) -> Cell {
    let g = grid.get();
    let start = Frame(frame.get() / g * g);
    let end = Frame(start.get().saturating_add(g));
    let b0 = map.beat_at_frame(start, rate).get();
    let b1 = map.beat_at_frame(end, rate).get();
    Cell {
        start,
        end,
        beat: Beat(b0),
        tempo: Bpm((b1 - b0) * 60.0 * rate.get() / g as f64),
    }
}
