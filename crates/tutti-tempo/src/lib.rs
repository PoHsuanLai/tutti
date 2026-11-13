#![doc = include_str!("../README.md")]
//! A tempo map: where beats fall in time.
//!
//! A [`TempoMap`] is an immutable value built on the control side from sorted
//! [`TempoPoint`]s. Each point holds a tempo until the next one
//! ([`Transition::Step`]) or moves to the next one's tempo
//! ([`Transition::RampTime`], tempo linear in time, or
//! [`Transition::RampBeats`], tempo linear in beats). Every query is a binary
//! search (or a hint hit) and a few flops: no allocation, no lock. A
//! [`TempoMapBuilder`] writes a map point by point and cannot end on a ramp.
//!
//! The map answers the queries that need no rate (beats and seconds). A reader
//! that walks it frame by frame takes a [`TempoView`] ([`TempoMap::view`]): the
//! map, the sample rate and a lookup hint, with the frame queries.
//!
//! Time is [`Seconds`](tutti_types::Seconds), an `f64`, so the map's figures
//! stay exact over hours: a seconds value in this crate is seconds from beat 0.
//!
//! The first point sits at beat 0. Before it the map holds the first tempo, so
//! a count-in has a time. Time signature is not here: it is
//! `tutti_types::MeterMap`.
//!
//! # Ramps as constant-tempo cells
//!
//! A reader that can only follow a constant tempo per stretch of frames takes the
//! map as [`Cell`]s on one fixed frame grid ([`CellGrid`]), cut also at the
//! frame of every point ([`TempoView::next_change`]). [`TempoView::chord_cell`] is a
//! pure function of the map, the rate, the absolute frame and the grid, so how a caller
//! splits time into blocks cannot change a cell: the same ramp lands on the same
//! frames at any buffer size. The crate has no way to derive a grid from a block
//! and takes none. Which grid to use, and how a render chunks a large block
//! across cells, is for the caller that renders.
//!
//! ```
//! use tutti_tempo::{TempoMap, TempoPoint};
//! use tutti_types::{Beat, Bpm, Frame, SampleRate};
//!
//! // 120 to 180 BPM over four beats, tempo linear in time, lasts 1.6 s.
//! let map = TempoMap::new([
//!     TempoPoint::ramp_time(Beat(0.0), Bpm::of(120.0)),
//!     TempoPoint::step(Beat(4.0), Bpm::of(180.0)),
//! ])?;
//! let rate = SampleRate::SR_48K;
//! assert_eq!(map.view(rate).frame_at_or_after(Beat(4.0)), Frame::new(76_800));
//! # Ok::<(), tutti_tempo::TempoError>(())
//! ```
#![forbid(unsafe_code)]

mod builder;
mod error;
mod grid;
mod map;
mod point;
mod segment;
mod view;

pub use builder::TempoMapBuilder;
pub use error::TempoError;
pub use grid::{Cell, CellGrid, CellGridError};
pub use map::TempoMap;
pub use point::{TempoPoint, Transition};
pub use view::TempoView;
