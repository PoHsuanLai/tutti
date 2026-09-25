//! Value vocabulary — what a parameter *is* and how it's carried.
//!
//! - [`units`] — the [`Unit`] marker trait + the measurement-unit newtypes
//!   (`Bpm`, `Hz`, `Db`, `Semitones`, …) plus [`AtomicSamplePosition`].
//!   "What kind of value this is."
//! - [`param`] — [`Param`], a lock-free atomic cell holding a `Unit` value,
//!   shareable with the audio thread (load / store / handle).
//! - [`frame`] — [`Frame`], an absolute frame position (a count's affine
//!   partner), and [`At`], when a scheduled command takes effect.
//! - [`timeline`] — [`TimelineSegment`], the one frame↔beat conversion, and
//!   [`first_frame_at_or_after`], the one rule for the frame a beat lands on.
//! - [`samples`] — [`Samples`], an integer frame count. A discrete quantity,
//!   deliberately *not* a `Unit` (it is compared and added, not interpolated
//!   or automated).

// `#[macro_use]` propagates the `unit_*` operator macros up to the crate root,
// so sibling modules (`meter`) can opt a type into an algebra instead of
// hand-writing it. Declared first for the same textual-scoping reason.
#[macro_use]
pub mod units;

pub mod cc_number;
pub mod midi_channel;
pub mod midi_group;
pub mod note;

pub mod frame;
pub mod latency;
pub mod param;
pub mod samples;
pub mod tail;
pub mod timeline;
pub mod unit_param;

pub use cc_number::CCNumber;
pub use frame::{At, Frame};
pub use latency::Latency;
pub use midi_channel::MidiChannel;
pub use midi_group::MidiGroup;
pub use note::{NotOnMidiScale, Note, NoteNumberOutOfRange, PitchClass};
pub use param::Param;
pub use samples::Samples;
pub use tail::Tail;
pub use timeline::{first_frame_at_or_after, TimelineSegment, FRAME_TOLERANCE};
pub use unit_param::{ParamAddr, ParamKey, UnitParam, UnitParamOutOfRange};
pub use units::{
    Amplitude, ArcDegrees, AtomicReadRate, AtomicSamplePosition, Azimuth, Beat, BeatDuration, Bpm,
    Cents, CompressionRatio, Confidence, Correlation, Db, Depth, Drive, Elevation, Feedback, Hz,
    Mix, Pan, Phase, PhaseIncrement, PlaybackRate, Radians, ReadRate, Resonance, SamplePosition,
    SampleRate, Seconds, Semitones, Spread, SrcRatio, StereoWidth, StretchFactor, Unit, Velocity,
    Q,
};
