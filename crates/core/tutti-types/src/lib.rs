//! Shared vocabulary for the Tutti audio engine.
//!
//! The common definitions every subsystem — `tutti-core`, the plugin host
//! crates, export, analysis — shares rather than duplicating. Four families,
//! one module each:
//!
//! **[`value`]** — what a parameter *is* and how it's carried: the [`Unit`]
//! marker trait + the measurement newtypes ([`Bpm`], [`Hz`], [`Db`], …), the
//! [`Param`] atomic cell that holds one, and the integer [`Samples`] count.
//!
//! **[`rt`]** — the RT-callback primitives the audio thread touches:
//! [`AudioThreadCell`] (one-borrow-at-a-time interior mutability), the capped
//! collections [`RtEventBuf`] (reached through `&self`) and [`RtVec`] (through
//! `&mut self`), the fixed-capacity [`RtScratch`] (own-and-slice, no push), and
//! the [`ScopedNoDenormals`] guard.
//!
//! **[`channels`]** — [`ChannelLayout`] (`Mono`/`Stereo`/`Multi(n)`): the one
//! answer to "mono, stereo, or how many?" that every subsystem shares instead of
//! a private enum or a bare channel-count integer.
//!
//! **[`io`]** — the I/O edge vocabulary: [`AudioIn`] / [`AudioOut`] — the two
//! traits every audio source and sink in the engine speaks (mic, file, disk,
//! plugin boundary), plus [`pump`](io::pump). Homed here, at the root leaf, so
//! every subsystem can implement them without an absurd dependency edge.
//!
//! **[`latency`]** — latency compensation: the [`LatencyGraph`] trait and the
//! [`plan`](latency::plan) / [`compensate`] algorithm that aligns unequal signal
//! paths, over the [`Samples`] count. Pure graph math with no audio dependency,
//! so any graph representation can drive it.
//!
//! **[`meter`]** — musical meter: [`TimeSignature`], the [`MeterMap`] timeline of
//! changes, and the [`Meter`] trait that turns a [`Beat`] into a bar and beat.
//! Pure musical math with no audio dependency, so it layers *over* a transport
//! rather than living inside one.
//!
//! Everything is re-exported at the crate root, so `tutti_types::AudioThreadCell`,
//! `tutti_types::Bpm`, `tutti_types::Samples`, etc. resolve directly.

// `value` is declared first and `#[macro_use]`d so the `unit_*` operator macros
// it defines are in scope for the modules below — `macro_rules!` are textually
// scoped, so a module declared *before* the one defining them cannot see them.
// `meter` hand-wrote ~90 lines of affine operators for exactly this reason.
//
// The macros stay crate-private rather than `#[macro_export]`ed: eight names as
// generic as `unit_bounded!` would sit permanently at the root of a published
// crate, un-renameable and un-feature-gateable. Every unit is now defined here,
// so nothing pays for that privacy in hand-written operators — `SampleRate` was
// the last holdout (it lived in `fundsp-tutti`, whose traits take it) until it
// moved here and `fundsp-tutti` switched to re-exporting it.
#[macro_use]
pub mod value;

pub mod channels;
pub mod downmix;
pub mod interleaved;
pub mod io;
pub mod latency;
pub mod meter;
pub mod pcm;
pub mod rt;
pub mod tail;

// RT-callback primitives.
pub use rt::{
    AudioThreadCell, BorrowGuard, BorrowRef, RtEventBuf, RtPublish, RtRef, RtScratch,
    RtScratchOverflow, RtVec, ScopedNoDenormals,
};

// Value vocabulary.
pub use value::{
    Amplitude, ArcDegrees, AtomicReadRate, AtomicSamplePosition, Azimuth, Beat, BeatDuration, Bpm,
    CCNumber, Cents, CompressionRatio, Confidence, Correlation, Db, Depth, Drive, Elevation,
    Feedback, Hz, MidiChannel, MidiGroup, Mix, NotOnMidiScale, Note, NoteNumberOutOfRange, Pan,
    Param, ParamAddr,
    Phase, PhaseIncrement, PitchClass, PlaybackRate, Radians, ReadRate, Resonance, SamplePosition,
    SampleRate, Samples, Seconds, Semitones, Spread, SrcRatio, StereoWidth, StretchFactor, Tail,
    Unit, UnitParam, UnitParamOutOfRange, Velocity, Q,
};

// Channel layout.
pub use channels::ChannelLayout;

// Surround → stereo / mono downmix matrices (ITU-R BS.775 / Dolby).
pub use downmix::{
    fold_buffer_to_mono, fold_frame, fold_frame_to_mono, fold_frame_to_stereo, fold_planar_to_mono,
};

// A flat buffer that carries its own frame width, so a frame index and a sample
// index stop being the same type.
pub use interleaved::{Interleaved, InterleavedMut, StereoPlanes};

// I/O edge + latency.
pub use io::{pump, AudioIn, AudioOut, OnEmpty};
pub use latency::{compensate, Compensation, DelayInsertion, LatencyGraph};

// How long a graph rings after its input stops.
pub use tail::{graph_tail, GraphTail, TailGraph};

// Musical meter.
pub use meter::{
    BarCount, BarNumber, BarPosition, BeatsPerBar, Meter, MeterChange, MeterMap, NoteValue,
    TimeSignature,
};

// PCM quantization.
pub use pcm::{f32_to_i16, f32_to_i24};
