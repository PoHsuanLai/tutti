//! The easing laws of the engine, and the automation trajectory built on them.
//!
//! A [`Law`] is a function `g` of progress `t` in `[0, 1]` with `g(0) = 0` and
//! `g(1) = 1`: how a value moves between two endpoints. It is a closed set of
//! six, one `Copy` value a few words long, so a parameter segment on the audio
//! thread carries one without an `Arc<dyn _>`. The graph's parameter ports, the
//! modulation shaping and the automation lanes all evaluate the same `Law`.
//!
//! A [`Piece`] is a value moving from one endpoint to another along a `Law`, read at
//! a progress: the one formula every consumer evaluates.
//!
//! An [`Trajectory`] is the least a lane or a recorder needs of
//! automation: breakpoints in [`Beat`](tutti_types::Beat), each with the `Law`
//! of the piece it starts, read at a beat.
//!
//! ```
//! use tutti_curve::{Trajectory, Breakpoint, Law, Piece};
//! use tutti_types::Beat;
//!
//! assert_eq!(Piece::new(0.0, 8.0, Law::PowerIn(2.0)).value(0.5), 2.0);
//!
//! let mut lane = Trajectory::new();
//! lane.add_breakpoint(Breakpoint::with_law(Beat(0.0), 0.0, Law::PowerIn(2.0)));
//! lane.add_breakpoint(Breakpoint::new(Beat(4.0), 1.0));
//! assert_eq!(lane.value_at(Beat(2.0)), Some(0.25));
//! ```
#![forbid(unsafe_code)]

mod law;
mod piece;
mod trajectory;

pub use law::Law;
pub use piece::Piece;
pub use trajectory::{Breakpoint, Trajectory};
