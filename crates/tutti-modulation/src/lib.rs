#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
//! Modulation for the tutti engine, as a **pure compiler**.
//! A host declares sources, routes and targets; [`compile()`] turns them into one
//! [`ControlProgram`] per modulated target, a description of each
//! signal-driven source's node, and the event edges between them. The
//! `tutti` facade re-exports it as `tutti::modulation` (feature `modulation`).
//!
//! ## What is here
//!
//! - **The compiler.** `compile(&Declarations, prev, now, rate)`: sources
//!   (`Source::Lfo`, `Macro`, `Follower`, `NoteEnvelope`), routes (`RouteDecl`,
//!   to a param or to another route's depth) and targets (`TargetDecl`: range
//!   and automation) in, a `Compiled` out. Pure: no clock, no state, no
//!   `ArcLend`. Equal declarations compile equal, so a host republishes only
//!   the programs that changed. Refusals are by name (`CompileError`).
//! - **The program.** `ControlProgram`: automation (which replaces the knob
//!   where it has a value) plus time-function layers and event-input layers
//!   (which add), summed in `LayerId` order. A description: equal programs
//!   evaluate to equal bits.
//! - **The source builders.** `Lfo` (`Lfo::sine().rate(BeatDuration(0.5))`),
//!   `Macro`, and the source halves of a follower (`Follower`: attack and
//!   release) and a note envelope (`NoteEnvelope`: the ADSR), each a
//!   `Source` through `Source::from`. What feeds a follower or a note envelope
//!   is the host's to wire.
//! - **The vocabulary.** `Curve` / `CurveSegment` (a value as a pure function
//!   of a beat: automation), `LfoShape`, `Polarity`, and the one shaping
//!   function every offset goes through, `shape(raw, depth, polarity, curve)`.
//!
//! ## What this crate does not own
//!
//! - **A graph runtime.** Nothing here holds a cell or knows the graph. The
//!   control sources' math is here (`ControlSource`: a value as legs on the play
//!   clock, `PlayGrid`; the follower's detector `LevelFollower` and the note
//!   envelope's `NoteAdsr`), but the node that walks the play clock and sends
//!   them (`SourceNode<S>`, aliased `EnvelopeFollowerNode` and
//!   `NoteEnvelopeNode`), the control lane that evaluates a program
//!   (`ControlLaneNode`) and the reconcile that wires them into a graph
//!   (`Modulation`) are `tutti-nodes`'.
//!
//! It depends only on `tutti-types` and `tutti-curve` (for `Law`).
//!
//! ## Example
//!
//! An LFO on a filter's cutoff, a quarter of the range deep:
//!
//! ```rust
//! use tutti_modulation::{
//!     compile, Clock, Law, Declarations, LfoShape, Polarity, ProgramTime, RouteDecl,
//!     RouteKey, RouteTarget, Source, SourceKey, TargetDecl, TargetKey, Waveform,
//! };
//! use tutti_types::{
//!     Beat, BeatsPerCycle, Depth, Frame, ParamAddr, PhaseIncrement, PlayReading, SamplePosition,
//!     SampleRate, UnitParam,
//! };
//!
//! let cutoff = TargetKey::param(7, ParamAddr::Unit(UnitParam::Cutoff));
//! let sources = [(
//!     SourceKey(1),
//!     Source::Lfo {
//!         wave: Waveform::Shape { shape: LfoShape::Square, seed: 0 },
//!         clock: Clock::Beats { per_cycle: BeatsPerCycle::of(4.0), phase: PhaseIncrement(0.0) },
//!     },
//! )];
//! let routes = [(
//!     RouteKey(1),
//!     RouteDecl {
//!         source: SourceKey(1),
//!         to: RouteTarget::Param(cutoff),
//!         depth: Depth::of(0.25),
//!         polarity: Polarity::Bipolar,
//!         curve: Law::Linear,
//!     },
//! )];
//! let targets = [(cutoff, TargetDecl { min: 200.0, max: 1_000.0, automation: None })];
//! let decl = Declarations { sources: &sources, routes: &routes, targets: &targets };
//! let rate = SampleRate::SR_48K;
//! let compiled = compile(&decl, None, PlayReading { epoch: 1, play: Frame::new(0) }, rate).unwrap();
//!
//! // One program, for the cutoff. Its lane evaluates it against the knob (the
//! // node's own cell) and the range's span (800): a square, ±200.
//! let program = &compiled.programs[&cutoff];
//! let at = |beat| ProgramTime {
//!     beat: Beat(beat),
//!     play: SamplePosition(0.0),
//!     play_epoch: 1,
//!     rate,
//! };
//! assert_eq!(program.value_at(Some(600.0), &at(1.0), 800.0, &[]), Some(800.0));
//! assert_eq!(program.value_at(Some(600.0), &at(3.0), 800.0, &[]), Some(400.0));
//! // An LFO needs no node and no edge: only signal sources do.
//! assert!(compiled.nodes.is_empty() && compiled.edges.is_empty());
//! ```
//!
//! ## Features
//!
//! `default = []`: everything above.

mod compile;
mod curve;
mod program;
mod shape;
mod source;
mod sources;

pub use compile::{
    CompileError, Compiled, Declarations, RATE_ANCHOR_MARGIN, RouteDecl, RouteKey, RouteTarget,
    SignalKind, SignalSource, Source, SourceKey, TargetDecl, TargetKey, compile, layer_id,
};
pub use curve::{Curve, CurveSegment};
pub use program::{
    Amount, Clock, ControlProgram, Corners, DepthBy, InputLayer, Layer, LayerId, MAX_LANE_INPUTS,
    MAX_LANE_LAYERS, PeriodPoint, ProgramError, ProgramTime, RateAnchor, SourceId, Waveform,
};
pub use shape::{LfoShape, Polarity, shape};
pub use source::{CONTROL_GRID, ControlSource, Leg, LevelFollower, NoteAdsr, PlayGrid};
pub use sources::{Follower, Lfo, LfoRate, Macro, NoteEnvelope};
pub use tutti_curve::{Curvature, Exponent, Law};
pub use tutti_types::Adsr;
