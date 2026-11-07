//! Value vocabulary — what a parameter *is* and how it's carried.
//!
//! - [`units`] — the [`Unit`] marker trait + the measurement-unit newtypes
//!   (`Bpm`, `Hz`, `Db`, `Semitones`, …). "What kind of value this is."
//! - [`param`] — [`Param`], a lock-free atomic cell holding a `Unit` value,
//!   shareable with the audio thread (load / store / handle), and
//!   [`Param64`], its `f64` neighbour.
//! - [`frame`] — [`Frame`], an absolute frame position (a count's affine
//!   partner), and [`At`], when a scheduled command takes effect.
//! - [`frame_clock`] — [`FrameClock`], the playhead as a frame count, and
//!   the [`LoopRange`] it wraps at.
//! - [`timeline`] — [`TimelineSegment`], the one frame↔beat conversion, and
//!   [`first_frame_at_or_after`], the one rule for the frame a beat lands on.
//! - [`play`] — [`PlayReading`], the graph's play clock at one frame.
//! - [`samples`] — [`Samples`], an integer frame count. A discrete quantity,
//!   deliberately *not* a `Unit` (it is compared and added, not interpolated
//!   or automated).

// `#[macro_use]` propagates `unit!` up to the crate root, so sibling modules
// (`meter`, `samples`, `frame`) define their quantities with it instead of
// hand-writing the type. Declared first for the same textual-scoping reason.
#[macro_use]
pub mod units;

pub mod adsr;
pub mod note;

pub mod frame;
pub mod frame_clock;
pub mod latency;
pub mod max_block;
pub mod param;
pub mod play;
pub mod samples;
pub mod tail;
pub mod timeline;
pub mod unit_param;

pub use adsr::Adsr;
pub use frame::{At, Frame};
pub use frame_clock::{FrameClock, LoopRange, Pass, PassEnd, SegmentOrigin};
pub use latency::Latency;
pub use max_block::{InvalidMaxBlock, MaxBlock};
pub use note::{NotOnMidiScale, Note, NoteNumberOutOfRange, PitchClass};
pub use param::{Param, Param64};
pub use play::PlayReading;
pub use samples::Samples;
pub use tail::Tail;
pub use timeline::{
    FRAME_TOLERANCE, TimelineSegment, beats_per_sample, first_frame_at_or_after,
    snap_to_whole_frame,
};
pub use unit_param::{ParamAddr, ParamKey, UnitAddr, UnitParam, UnitParamOutOfRange};
pub use units::{
    Amplitude, ArcDegrees, Azimuth, Beat, BeatDuration, Bpm, Cents, CompressionRatio, Confidence,
    Correlation, Db, Depth, Drive, Elevation, Feedback, Hz, Mix, Pan, Phase, PhaseIncrement,
    PlaybackRate, Q, Radians, ReadRate, Resonance, SamplePosition, SampleRate, Seconds, Semitones,
    Spread, SrcRatio, StereoWidth, StretchFactor, Unit, Velocity,
};
