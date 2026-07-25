//! Value vocabulary — what a parameter *is* and how it's carried.
//!
//! - [`units`] — the [`Unit`] marker trait + the measurement-unit newtypes
//!   (`Bpm`, `Hz`, `Db`, `Semitones`, …) plus [`AtomicSamplePosition`].
//!   "What kind of value this is."
//! - [`param`] — [`Param`], a lock-free atomic cell holding a `Unit` value,
//!   shareable with the audio thread (load / store / handle).
//! - [`samples`] — [`Samples`], an integer frame count. A discrete quantity,
//!   deliberately *not* a `Unit` (it is compared and added, not interpolated
//!   or automated).

// `#[macro_use]` propagates the `unit_*` operator macros up to the crate root,
// so sibling modules (`meter`) can opt a type into an algebra instead of
// hand-writing it. Declared first for the same textual-scoping reason.
#[macro_use]
pub mod units;

pub mod param;
pub mod samples;
pub mod unit_param;

pub use param::Param;
pub use samples::Samples;
pub use unit_param::{ParamAddr, UnitParam, UnitParamOutOfRange};
pub use units::{
    ArcDegrees, AtomicSamplePosition, Azimuth, Beat, BeatDuration, Bpm, Cents, Db, Elevation, Hz,
    Linear, Phase, PhaseIncrement, PlaybackRate, Radians, Ratio, SamplePosition, Seconds,
    Semitones, SrcRatio, StretchFactor, Unit,
};
