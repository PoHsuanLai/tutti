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
//! [`AudioThreadCell`] (one-borrow-at-a-time interior mutability), [`RtEventBuf`]
//! (fixed-inline event collector), [`RtScratchBuf`] (fill-then-lend) and its
//! sibling [`RtScratch`] (own-and-slice), and the [`ScopedNoDenormals`] guard.
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
//! Everything is re-exported at the crate root, so `tutti_types::AudioThreadCell`,
//! `tutti_types::Bpm`, `tutti_types::Samples`, etc. resolve directly.

pub mod io;
pub mod latency;
pub mod pcm;
pub mod rt;
pub mod value;

// RT-callback primitives.
pub use rt::{
    AudioThreadCell, BorrowGuard, BorrowRef, RtEventBuf, RtScratch, RtScratchBuf,
    RtScratchOverflow, ScopedNoDenormals,
};

// Value vocabulary.
pub use value::{
    AtomicSamplePosition, Beat, BeatDuration, Bpm, Cents, Db, Degrees, Hz, Linear, Param, Ratio,
    SamplePosition, Samples, Seconds, Semitones, Unit, UnitParam, UnitParamOutOfRange,
};

// I/O edge + latency.
pub use io::{pump, AudioIn, AudioOut};
pub use latency::{compensate, Compensation, DelayInsertion, LatencyGraph};

// PCM quantization.
pub use pcm::{f32_to_i16, f32_to_i24};
