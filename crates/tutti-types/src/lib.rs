#![doc = include_str!("../README.md")]
//! Typed quantities and shapes shared by every engine crate: unit newtypes,
//! channel layouts, and the `Source`/`Sink` I/O traits.
//!
//! ## What this is
//!
//! The common definitions every other Tutti crate shares rather than duplicating.
//! Use it directly when you need only the vocabulary (a crate that implements
//! `Source`, or one that only needs `Beat`); otherwise reach it through the
//! `tutti` umbrella, which re-exports it as `tutti::types`. What is in it:
//!
//! - **`value`** — what a parameter *is* and how it is carried: the `Unit` marker
//!   trait, the measurement newtypes (`Hz`, `Seconds`, `Db`, `Beat`, `Bpm`,
//!   `Samples`, …), and the atomic `Param<U>` cell that holds one.
//! - **Channels** — `ChannelLayout`, a `u16` channel count with named constants
//!   (`MONO`, `STEREO`, ...): the one answer to "how many channels?" that every
//!   subsystem shares instead of a bare integer. `ChannelTopology` + `Speaker`
//!   answer the *different* question of which speaker each channel feeds; distinct
//!   questions, so distinct types.
//! - **Buffers** — `Interleaved` / `InterleavedMut`, a flat buffer that carries its
//!   own frame width, so a frame index and a sample index stop being the same type.
//!   `Fold::between(&src, &dst)` turns one `ChannelTopology` into another by
//!   speaker (ITU-R BS.775 / Dolby coefficients, a pure permutation for the same
//!   speakers in another order, unassigned channels one to one) and refuses a channel
//!   it cannot place; the matrix is not normalised. `Fold::widths` / `Fold::to_width`
//!   are the entry points for an edge that knows only a width (a device, an export
//!   file, the graph root): the engine's order where the width has one, channels by
//!   index (`Fold::by_index`, widening only; a narrowing is `FoldError::Unplaced`,
//!   and `Fold::select` truncates on purpose) where it has none. The width-only folds (`fold_frame`, `fold_frame_to_mono`,
//!   `fold_frame_to_stereo`, `fold_buffer_to_mono`, `fold_planar_to_mono`) are gone.
//! - **`io`** — `Source`, `Sink` and `Pull`: the edge vocabulary, a stream on a clock
//!   other than the graph's (mic, file, tap, file encoder). A `Pull` says whether an
//!   empty read is late, finished or broken.
//! - **`meter`** — musical meter: `TimeSignature`, the `MeterMap` timeline of
//!   changes, and the `Meter` trait that turns a `Beat` into a bar and beat. Pure
//!   musical math, so it layers *over* a transport rather than living inside one.
//!
//! **The root is the API.** The common names are re-exported there, so
//! `tutti_types::Bpm` and `tutti_types::Source` resolve directly — import from the
//! root. Three of the eight modules are
//! public (`value`, `io`, `meter`), because each holds items the root does not
//! re-export; the others (`channels`, `downmix`, `fold`, `interleaved`, `topology`)
//! are private and fully re-exported. The graph as a value (`Topology`), the
//! latency pass and the tail fold are `tutti-graph`'s.
//!
//! ## What this crate does not own
//!
//! Anything that renders, decodes, or opens a device. It is the **root leaf** —
//! Engine-free, depending on no workspace crate (its outside
//! dependencies are `atomic_float` and `libm`, plus `serde`
//! behind its feature) and `#![forbid(unsafe_code)]`. The lock-free and `unsafe`
//! runtime primitives, the graph runtime, the engine, the device and the file
//! codecs are other crates'. Two consequences make the split load-bearing:
//!
//! - The `Source`/`Sink` traits are homed here so *any* subsystem can implement
//!   them without an absurd dependency edge. A crate implementing `Source` must
//!   not have to depend on the device layer to name the trait.
//! - Unit newtypes are **mandatory** across the engine, so their home has to be
//!   reachable from everywhere. A crate that only needs `Beat` gets `Beat` and
//!   nothing else.
//!
//! ## Example — the measurement vocabulary
//!
//! This crate depends on no other engine crate, so there is nothing to integrate
//! with: what a consumer meets first is the units. Cross a family boundary with the
//! **named converter**, never with arithmetic on the inner float.
//!
//! ```rust
//! use tutti_types::{Cents, Db, SampleRate, Samples, Seconds, Semitones};
//!
//! // dB → linear gain. `Db` is logarithmic, so this is a conversion, not a cast.
//! let trim = Db(-6.0);
//! assert!((trim.to_amplitude().get() - 0.501_187).abs() < 1e-5);
//!
//! // Cascaded gain stages ADD in dB — that operator is opted in because it
//! // means something. Multiplication is NOT: scaling the dB value squares the
//! // amplitude, so `trim * 2.0` would be a quarter of the signal, not half of
//! // it. `Db` has no `Mul`; a factor of two lives in the amplitude domain.
//! assert_eq!(trim + trim, Db(-12.0));
//! let quartered = Db(-12.0).to_amplitude().get();
//! let squared = trim.to_amplitude().get() * trim.to_amplitude().get();
//! assert!((quartered - squared).abs() < 1e-6);
//!
//! // Seconds → frames. The rounding is IN THE NAME because allocating a delay
//! // line and counting elapsed frames want different answers from one span.
//! let rate = SampleRate::SR_48K;
//! let block = Seconds(0.010_51); // 504.48 frames
//! assert_eq!(block.to_samples_floor(rate), Samples(504)); // frames elapsed
//! assert_eq!(block.to_samples_ceil(rate), Samples(505)); // frames to allocate
//!
//! // Pitch offsets convert too — `cents.get() / 100.0` would compile and return
//! // `Cents`, a value wrong by 100x whose type claims it is fine.
//! assert_eq!(Cents(1200.0).to_semitones(), Semitones(12.0));
//! ```
//!
//! ## Units are labels
//!
//! A unit is a name on a float, not a validated value: `Pan(5.0)` builds. The
//! range a type documents is what its `new_clamped` or wrap produces, and
//! `new_clamped` passes NaN through (see the `units` module docs). The types that
//! do validate are `SampleRate`, `Bpm`, `Depth`, `LoopRange`, `BeatsPerBar`,
//! `NoteValue` and `MeterMap`.
//! `SampleRate::new` (finite and above zero), `LoopRange::new`, `BeatsPerBar::new` and `NoteValue::new` return `None` for a
//! raw they cannot hold (`BeatsPerBar::clamped` is the one named saturation);
//! `MeterMap::new` repairs its list (drops non-finite beats, sorts, dedups).
//!
//! ## Operators are opt-in, and every omission is deliberate
//!
//! A unit gets exactly the algebra it has. `Db * 2.0`, `Beat + Beat` and
//! `Azimuth: Ord` do not exist, because each would compile and mean the wrong
//! thing (a squared amplitude, two positions added with no origin, an ordering
//! on a circle). Each type's docs name what it omits and the named method that
//! replaces it, so the answer to a missing operator is always a method, never an
//! escape to raw `f32`.
//!
//! ## Features
//!
//! Off by default (`default = []`).
//!
//! - `serde` — `Serialize`/`Deserialize` on the wire- and document-carried types:
//!   the unit newtypes, `Samples`, `Frame`, `Note`, `Latency`, `Tail`,
//!   `ChannelLayout`, `ChannelTopology` and the meter vocabulary. Not `Param`,
//!   `UnitParam` or `LoopRange`.
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
mod bus;
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
    Adsr, Amplitude, ArcDegrees, At, AtomicSampleRate, Azimuth, Beat, BeatDuration, BeatsPerCycle,
    BeatsPerCycleOutOfRange, Bpm, BpmOutOfRange, Cents, CompressionRatio, Confidence, Correlation,
    Db, Depth, DepthOutOfRange, Direction, Drive, Elevation, Feedback, Frame, Hz, InvalidMaxBlock,
    Latency, MaxBlock, Mix, Moment, NotOnMidiScale, Note, NoteNumberOutOfRange, Pan, Param,
    Param64, ParamAddr, ParamCell, ParamKey, ParamRaw, Phase, PhaseIncrement, PitchClass,
    PlayReading, PlaybackRate, Q, Radians, ReadRate, Resonance, SamplePosition, SampleRate,
    SampleRateOutOfRange, Samples, Seconds, Semitones, Spread, SrcRatio, StereoBalance,
    StereoWidth, StretchFactor, Tail, Unit, UnitAddr, UnitParam, UnitParamOutOfRange, Velocity,
};

// The frame is the source of truth for time: the one frame↔beat conversion,
// and the one rule for which frame a beat lands on.
pub use value::{
    FRAME_TOLERANCE, FrameClock, LoopRange, LoopRangeError, Pass, PassEnd, SegmentOrigin,
    TimelineSegment, beats_per_sample, first_frame_at_or_after, frames_per_beat,
    snap_to_whole_frame,
};

// Channel layout — how many channels.
pub use bus::{BusTooWide, BusTopology, MAX_BUS_CHANNELS};
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
pub use io::{FiniteSource, Pull, Sink, SinkFailed, Source, Stop};

// Musical meter.
pub use meter::{
    BarCount, BarNumber, BarPosition, BeatsPerBar, InvalidBeatsPerBar, InvalidNoteValue, Meter,
    MeterChange, MeterMap, NoteValue, TimeSignature,
};
