#![doc = include_str!("../README.md")]
// Turned on after it found a real one: `RtEventBuf`'s entire struct-level doc
// comment was attached to the `Debug` impl below it, so the type itself was
// undocumented in rustdoc while looking thoroughly documented in the source.
// That is the failure mode this lint exists for, and nothing else catches it.
#![deny(missing_docs)]

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

// Private: everything public in these is re-exported at the root below, so the
// module path would be a second name for a type that already has one.
mod channels;
mod downmix;
mod interleaved;
mod rt;
mod topology;

// Public: `tutti-core` re-exports each of these AS A MODULE
// (`pub use tutti_types::io::{self, ...}`), so consumers spell
// `tutti_core::io::AudioIn`. Privatizing one here breaks that path.
pub mod graph;
pub mod io;
pub mod latency;
pub mod meter;
pub mod pcm;
pub mod tail;

// RT-callback primitives.
pub use rt::{
    AudioThread, AudioThreadCell, AudioThreadGuard, BorrowGuard, BorrowRef, Guarded, Retire,
    RtEventBuf, RtPublish, RtRef, RtScratch, RtScratchOverflow, RtVec, ScopedNoDenormals,
};

// Value vocabulary.
pub use value::{
    Amplitude, ArcDegrees, AtomicReadRate, AtomicSamplePosition, Azimuth, Beat, BeatDuration, Bpm,
    CCNumber, Cents, CompressionRatio, Confidence, Correlation, Db, Depth, Drive, Elevation,
    Feedback, Hz, Latency, MidiChannel, MidiGroup, Mix, NotOnMidiScale, Note, NoteNumberOutOfRange,
    Pan, Param, ParamAddr, ParamKey, Phase, PhaseIncrement, PitchClass, PlaybackRate, Radians,
    ReadRate, Resonance, SamplePosition, SampleRate, Samples, Seconds, Semitones, Spread, SrcRatio,
    StereoWidth, StretchFactor, Tail, Unit, UnitParam, UnitParamOutOfRange, Velocity, Q,
};

// Channel layout — how many channels.
pub use channels::ChannelLayout;

// Channel topology — which speaker each channel feeds. Distinct from the count
// above, and additive to it: a width-only caller keeps using `ChannelLayout`.
pub use topology::{ChannelTopology, Speaker};

// Surround → stereo / mono downmix matrices (ITU-R BS.775 / Dolby). `M3DB` is
// the −3 dB centre/surround coefficient those matrices apply, and it comes along
// because a caller checking a fold's output has to name the same constant.
pub use downmix::{
    fold_buffer_to_mono, fold_frame, fold_frame_to_mono, fold_frame_to_stereo, fold_planar_to_mono,
    M3DB,
};

// A flat buffer that carries its own frame width, so a frame index and a sample
// index stop being the same type.
pub use interleaved::{Interleaved, InterleavedMut, StereoPlanes};

// I/O edge + latency.
pub use io::{pump, AudioIn, AudioOut, OnEmpty};
pub use latency::{compensate, Compensation, DelayInsertion, LatencyGraph};

// How long a graph rings after its input stops.
pub use tail::{graph_tail, GraphTail, TailGraph};

// The audio graph as a value. Only the four names a consumer spells outside a
// builder come to the root: `Source`, `Edge`, `InPort`, `OutPort`, `NodeSpec`
// and `Invalid` stay module-qualified, because each is a generic word that reads
// wrong unprefixed — `graph::Source` is an edge's origin, and a root `Source`
// beside `io::AudioIn` would be read as an input device.
pub use graph::{NodeKey, Topology, Valid};

// Musical meter.
pub use meter::{
    BarCount, BarNumber, BarPosition, BeatsPerBar, Meter, MeterChange, MeterMap, NoteValue,
    TimeSignature,
};

/// The names a consumer of this crate actually reaches for, in one import.
///
/// Not here, and spelled in full instead: `Unit` (it collides with unrelated
/// `Unit` types in DSP crates), the `latency`/`tail` graph traits, and the error
/// types `NotOnMidiScale` / `UnitParamOutOfRange`.
pub mod prelude {
    pub use crate::value::{
        Amplitude, Beat, BeatDuration, Bpm, CCNumber, Cents, Db, Depth, Hz, MidiChannel, MidiGroup,
        Note, Param, ParamAddr, Phase, PhaseIncrement, PitchClass, SamplePosition, SampleRate,
        Samples, Seconds, Semitones, Tail, UnitParam, Velocity, Q,
    };

    pub use crate::channels::ChannelLayout;
    pub use crate::interleaved::{Interleaved, InterleavedMut, StereoPlanes};
    pub use crate::topology::{ChannelTopology, Speaker};

    pub use crate::io::{pump, AudioIn, AudioOut, OnEmpty};

    pub use crate::meter::{MeterMap, NoteValue, TimeSignature};
    pub use crate::rt::{RtPublish, RtRef};
}

// PCM quantization.
pub use pcm::{f32_to_i16, f32_to_i24};
