#![doc = include_str!("../README.md")]
// Catches a doc comment attached to the wrong item (say, an `impl Debug` right
// below a struct), which leaves the type undocumented in rustdoc while it looks
// documented in the source. Nothing else catches that.
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

// `value` is declared first and `#[macro_use]`d so `unit!`, the macro that
// defines every quantity, is in scope for the modules below — `macro_rules!`
// are textually scoped, so a module declared *before* the one defining them
// cannot see them.
//
// The macro stays crate-private rather than `#[macro_export]`ed: a name as
// generic as `unit!` would sit permanently at the root of a published crate,
// un-renameable and un-feature-gateable.
#[macro_use]
pub mod value;

// Private: everything public in these is re-exported at the root below, so the
// module path would be a second name for a type that already has one.
mod channels;
mod fold;
mod interleaved;
mod topology;

// Public: `io` holds items the root does not re-export.
pub mod io;
pub mod meter;

// The cell a `Param` shares (`Param::as_atomic`), named here so a crate that
// addresses params without their unit types need not depend on `atomic_float`
// itself.
pub use atomic_float::{AtomicF32, AtomicF64};

// Value vocabulary.
pub use value::{
    Adsr, Amplitude, ArcDegrees, At, Azimuth, Beat, BeatDuration, Bpm, BpmOutOfRange, Cents,
    CompressionRatio, Confidence, Correlation, Db, Depth, Drive, Elevation, Feedback, Frame, Hz,
    InvalidMaxBlock, Latency, MaxBlock, Mix, Moment, NotOnMidiScale, Note, NoteNumberOutOfRange,
    Pan, Param, Param64, ParamAddr, ParamCell, ParamKey, ParamRaw, Phase, PhaseIncrement,
    PitchClass, PlayReading, PlaybackRate, Q, Radians, ReadRate, Resonance, SamplePosition,
    SampleRate, Samples, Seconds, Semitones, Spread, SrcRatio, StereoWidth, StretchFactor, Tail,
    Unit, UnitAddr, UnitParam, UnitParamOutOfRange, Velocity,
};

// The frame is the source of truth for time: the one frame↔beat conversion,
// and the one rule for which frame a beat lands on.
pub use value::{
    FRAME_TOLERANCE, FrameClock, LoopRange, Pass, PassEnd, SegmentOrigin, TimelineSegment,
    beats_per_sample, first_frame_at_or_after, frames_per_beat, snap_to_whole_frame,
};

// Channel layout — how many channels.
pub use channels::{ChannelCountOutOfRange, ChannelLayout};

// Channel topology — which speaker each channel feeds. Distinct from the count
// above, and additive to it: a width-only caller keeps using `ChannelLayout`.
pub use topology::{ChannelTopology, Speaker};

// How one topology becomes another: the one fold, built on the control side.
// `M3DB` is the −3 dB centre/surround coefficient its ITU-R BS.775 / Dolby
// matrices apply, and it comes along because a caller checking a fold's output
// has to name the same constant.
pub use fold::{Fold, FoldEnd, FoldError, M3DB};

// A flat buffer that carries its own frame width, so a frame index and a sample
// index stop being the same type.
pub use interleaved::{Interleaved, InterleavedMut, StereoPlanes};

// I/O edge.
pub use io::{Pull, Sink, SinkFailed, Source, Stop};

// Musical meter.
pub use meter::{
    BarCount, BarNumber, BarPosition, BeatsPerBar, InvalidBeatsPerBar, InvalidNoteValue, Meter,
    MeterChange, MeterMap, NoteValue, TimeSignature,
};

/// The names a consumer of this crate actually reaches for, in one import.
///
/// Not here, and spelled in full instead: `Unit` (it collides with unrelated
/// `Unit` types in DSP crates), the `latency`/`tail` graph traits, and the error
/// types `NotOnMidiScale` / `UnitParamOutOfRange` / `BpmOutOfRange`.
pub mod prelude {
    pub use crate::value::{
        Amplitude, Beat, BeatDuration, Bpm, Cents, Db, Depth, Frame, Hz, Moment, Note, Param,
        ParamAddr, Phase, PhaseIncrement, PitchClass, Q, SamplePosition, SampleRate, Samples,
        Seconds, Semitones, Tail, UnitAddr, UnitParam, Velocity,
    };

    pub use crate::channels::ChannelLayout;
    pub use crate::interleaved::{Interleaved, InterleavedMut, StereoPlanes};
    pub use crate::topology::{ChannelTopology, Speaker};

    pub use crate::io::{Pull, Sink, SinkFailed, Source, Stop};

    pub use crate::meter::{MeterMap, NoteValue, TimeSignature};
}
