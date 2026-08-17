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
//! **RT primitives** — what the audio thread touches:
//! [`AudioThreadCell`] (one-borrow-at-a-time interior mutability), the capped
//! collections [`RtEventBuf`] (reached through `&self`) and [`RtVec`] (through
//! `&mut self`), the fixed-capacity [`RtScratch`] (own-and-slice, no push), and
//! the [`ScopedNoDenormals`] guard.
//!
//! **Channels** — [`ChannelLayout`] (`Mono`/`Stereo`/`Multi(n)`): the one
//! answer to "mono, stereo, or how many?" that every subsystem shares instead of
//! a private enum or a bare channel-count integer.
//!
//! **[`io`]** — the I/O edge vocabulary: [`AudioIn`] / [`AudioOut`] — the two
//! traits every audio source and sink in the engine speaks (mic, file, disk,
//! plugin boundary), plus [`pump`]. Homed here, at the root leaf, so
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
//! `tutti_types::Bpm`, `tutti_types::Samples`, etc. resolve directly. **The root
//! is the API** — import from it, or from [`prelude`] for the common subset, and
//! not through a module path. Most modules here are private for that reason; the
//! ones that stay public do so because `tutti-core` re-exports them as modules.
//!
//! # Example — the measurement vocabulary
//!
//! Nothing above this crate in the stack, so there is nothing to integrate
//! with: what a consumer meets first is the units. Cross a family boundary with
//! the **named converter**, never with arithmetic on the inner float.
//!
//! ```
//! use tutti_types::{Cents, Db, SampleRate, Samples, Seconds, Semitones};
//!
//! // dB → linear gain. `Db` is logarithmic, so this is a conversion, not a cast.
//! let trim = Db(-6.0);
//! assert!((trim.to_amplitude().get() - 0.501_187).abs() < 1e-5);
//!
//! // Cascaded gain stages ADD in dB — that operator is opted in because it
//! // means something. Multiplication is NOT, and this is why: scaling the dB
//! // value squares the amplitude, so `trim * 2.0` would be a quarter of the
//! // signal, not half of it. The omission ledger withholds `Mul` and points at
//! // the amplitude domain, which is where a factor of two actually lives.
//! assert_eq!(trim + trim, Db(-12.0));
//! let quartered = Db(-12.0).to_amplitude().get();
//! let squared = trim.to_amplitude().get() * trim.to_amplitude().get();
//! assert!((quartered - squared).abs() < 1e-6);
//!
//! // Seconds → frames. The rounding is IN THE NAME because allocating a delay
//! // line and counting elapsed frames want different answers from one span.
//! let rate = SampleRate(48_000.0);
//! let block = Seconds(0.010_5);
//! assert_eq!(block.to_samples(rate), Samples(504));
//! assert_eq!(block.to_samples_ceil(rate), Samples(504));
//!
//! // Pitch offsets convert too — `cents.get() / 100.0` would compile and return
//! // `Cents`, a value wrong by 100x whose type claims it is fine.
//! assert_eq!(Cents(1200.0).to_semitones(), Semitones(12.0));
//! ```

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
pub mod io;
pub mod latency;
pub mod meter;
pub mod pcm;
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
    Param, ParamAddr, Phase, PhaseIncrement, PitchClass, PlaybackRate, Radians, ReadRate,
    Resonance, SamplePosition, SampleRate, Samples, Seconds, Semitones, Spread, SrcRatio,
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

// Musical meter.
pub use meter::{
    BarCount, BarNumber, BarPosition, BeatsPerBar, Meter, MeterChange, MeterMap, NoteValue,
    TimeSignature,
};

/// The names a consumer of this crate actually reaches for, in one import.
///
/// A curated subset of the crate root — it introduces no name the root lacks and
/// defines nothing, so `tutti_types::Beat` and `tutti_types::prelude::Beat` are
/// one path plus a shorthand rather than two paths. Membership was taken from
/// the callsites in this repo: everything imported from at least three places.
///
/// Deliberately absent: `Unit` (the marker trait — collides with unrelated
/// `Unit` types in DSP crates), the `latency`/`tail` graph traits (a consumer
/// implements those rarely and deliberately), and the error types
/// (`NotOnMidiScale`, `UnitParamOutOfRange`), which are named at the one call
/// that can fail rather than blanket-imported.
pub mod prelude {
    // Measurement vocabulary. `Beat`/`BeatDuration` lead because position and
    // span are the two most-imported names in the engine, and the pair is what
    // makes a musical signature spellable at all.
    pub use crate::value::{
        Amplitude, Beat, BeatDuration, Bpm, CCNumber, Cents, Db, Depth, Hz, MidiChannel, MidiGroup,
        Note, Param, ParamAddr, Phase, PhaseIncrement, PitchClass, SamplePosition, SampleRate,
        Samples, Seconds, Semitones, Tail, UnitParam, Velocity, Q,
    };

    // How many channels, which speaker each one feeds, and the flat buffer that
    // carries its own width. A signature naming any one of these usually names
    // the others.
    pub use crate::channels::ChannelLayout;
    pub use crate::interleaved::{Interleaved, InterleavedMut, StereoPlanes};
    pub use crate::topology::{ChannelTopology, Speaker};

    // The I/O edge. `OnEmpty` rides along because it is an associated const on
    // `AudioIn`: an implementor cannot write the impl without naming it.
    pub use crate::io::{pump, AudioIn, AudioOut, OnEmpty};

    // Musical meter, and the RT publish pair for non-scalar state.
    pub use crate::meter::{MeterMap, NoteValue, TimeSignature};
    pub use crate::rt::{RtPublish, RtRef};
}

// PCM quantization.
pub use pcm::{f32_to_i16, f32_to_i24};
