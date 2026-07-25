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

pub mod param;
pub mod samples;
pub mod unit_param;
pub mod units;

pub use param::Param;
pub use samples::Samples;
pub use unit_param::{ParamAddr, UnitParam, UnitParamOutOfRange};
pub use units::{
    AtomicSamplePosition, Beat, BeatDuration, Bpm, Cents, Db, Degrees, Hz, Linear, PlaybackRate,
    Ratio, SamplePosition, Seconds, Semitones, SrcRatio, StretchFactor, Unit,
};
