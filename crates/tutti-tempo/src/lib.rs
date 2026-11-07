//! A tempo map: where beats fall in time.
//!
//! A [`TempoMap`] is an immutable value built on the control side from sorted
//! [`TempoPoint`]s. Each point holds a tempo until the next one ([`Shape::Step`])
//! or moves to the next one's tempo ([`Shape::RampTime`], tempo linear in time,
//! or [`Shape::RampBeats`], tempo linear in beats). Every query is a binary
//! search (or a [`Cursor`] hit) and a few flops: no allocation, no lock.
//!
//! Seconds are plain `f64` here. `Seconds` is `f32`, which resolves a single
//! frame only to a few hundred seconds at 48 kHz; the map's figures stay exact
//! over hours. That is one of the places the unit types stop (`CLAUDE.md`,
//! "Unit types"), so a seconds value in this crate is `f64` seconds from beat 0.
//!
//! The first point sits at beat 0. Before it the map extrapolates at the first
//! tempo, so a count-in has a time. Time signature is not here: it is
//! `tutti_types::MeterMap`.
//!
//! # Ramps as constant-tempo cells
//!
//! A reader that can only follow a constant tempo per stretch of frames takes a
//! ramp as [`Cell`]s on one fixed frame grid ([`CellGrid`]). [`chord_cell`] is a
//! pure function of the map, the absolute frame and the grid, so how a caller
//! splits time into blocks cannot change a cell: the same ramp lands on the same
//! frames at any buffer size. The crate has no way to derive a grid from a block
//! and takes none. Which grid to use, and how a render chunks a large block
//! across cells, is for the caller that renders.
//!
//! ```
//! use tutti_tempo::{Shape, TempoMap, TempoPoint};
//! use tutti_types::{Beat, Bpm, Frame, SampleRate};
//!
//! // 120 to 180 BPM over four beats, tempo linear in time, lasts 1.6 s.
//! let map = TempoMap::new([
//!     TempoPoint { beat: Beat(0.0), tempo: Bpm(120.0), shape: Shape::RampTime },
//!     TempoPoint { beat: Beat(4.0), tempo: Bpm(180.0), shape: Shape::Step },
//! ])?;
//! let rate = SampleRate(48_000.0);
//! assert_eq!(map.frame_at_or_after(Beat(4.0), rate), Frame(76_800));
//! # Ok::<(), tutti_tempo::TempoError>(())
//! ```
#![forbid(unsafe_code)]

mod error;
mod grid;
mod map;
mod point;
mod segment;

pub use error::TempoError;
pub use grid::{Cell, CellGrid, CellGridError, chord_cell};
pub use map::{Cursor, TempoMap};
pub use point::{Shape, TempoPoint};
