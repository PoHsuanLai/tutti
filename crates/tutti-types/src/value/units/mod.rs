//! Unit newtypes for DSP and transport parameters.
//!
//! Each newtype is `#[repr(transparent)]` around its raw float type (`f32` by
//! default, `f64` for wide-range values like `Bpm`/`Beat`) — zero-cost at
//! runtime, but distinct at compile time so `Hz` and `Seconds` cannot be
//! swapped by accident. The [`Unit`] marker trait carries the raw type as an
//! associated type so [`Param`](super::Param) can be generic over the unit.
//!
//! [`SampleRate`] is defined here too, the one type a rate is spelled with.
//!
//! # Units are labels
//!
//! A unit is a name on a float, not a validated value: `Pan(5.0)` and
//! `Mix(-3.0)` build through the public field, `new` and `From`. The range a
//! type documents is what its named constructors and methods produce
//! (`new_clamped`, `wrapped`, `wrap`), not what a hand-built value holds.
//!
//! # Non-finite input
//!
//! One rule for every `new_clamped`: a NaN comes out as NaN, and an infinity
//! saturates at the bound on its side (an unbounded side stays infinite). A
//! constructor never invents a finite value for a NaN, so a caller that guards
//! non-finite values downstream, by holding the last good one, still sees the
//! NaN and can do so. Where no finite value must reach a cell, the caller
//! guards before the constructor.
//!
//! The wraps ([`Phase::wrapped`], [`Azimuth::wrap`], [`Beat::fract`],
//! [`SamplePosition::fract`], [`BeatDuration::rem_euclid`]) give their
//! half-open range for every finite input, and NaN for NaN or an infinite
//! input, where the range cannot hold. [`Radians::wrapped_signed`] is one of
//! them, with `pi` the `f32` constant.
//!
//! # Operators are opt-in, and every omission is deliberate
//!
//! A unit gets exactly the algebra it has. `Db * 2.0` squares the amplitude
//! rather than doubling the gain; `Beat + Beat` adds two timeline positions and
//! has no origin to add them about; [`Azimuth`] has no ordering because a circle
//! has no ends. Each type's own docs name the omission and the method that
//! replaces it, and the ledger is enumerated in this module's tests.
//!
//! **An omission must ship with its replacement.** Naming a substitute method
//! and not writing it is worse than the operator: call sites escape to raw
//! floats, and the type stops being consulted at all.
//!
//! # Measurements are not controls
//!
//! [`Correlation`], [`Pan`] and [`Confidence`] are values the engine *reports*;
//! [`Depth`] and [`Mix`] are values a user *sets*. The ranges coincide and the
//! meanings do not, so they are distinct types — swapping a reading for a
//! control is otherwise silent. [`Q`] and [`Resonance`] split on the same line.

#[macro_use]
mod macros;

mod amounts;
mod angles;
mod level;
mod measures;
mod ratios;
mod time;
mod transport;

// The frame-count vocabulary is integer-backed and lives next door in
// `samples.rs`; the families name it through this import.
use super::samples::Samples;

pub use amounts::{Depth, Drive, Feedback, Mix, Spread, StereoWidth};
pub use angles::{ArcDegrees, Azimuth, Elevation, Phase, PhaseIncrement, Radians};
pub use level::{Amplitude, Db};
pub use measures::{Confidence, Correlation, Pan, Velocity};
pub use ratios::{Cents, CompressionRatio, Q, Resonance, Semitones};
pub use time::{Hz, SampleRate, Seconds};
pub use transport::{
    Beat, BeatDuration, Bpm, PlaybackRate, ReadRate, SamplePosition, SrcRatio, StretchFactor,
};

/// Marker trait implemented by every unit newtype.
///
/// `Raw` is the underlying float representation. Most units use `f32`;
/// `SampleRate` uses `f64` because sample rates need a wider range and
/// precision than `f32` comfortably provides.
pub trait Unit: Copy + Clone + core::fmt::Debug + PartialEq {
    /// The underlying float representation — `f32` for most units, `f64` where
    /// the range or precision demands it.
    type Raw: Copy;
    /// Wraps a raw float, asserting it is already in this unit's denomination.
    ///
    /// No conversion and no validation: a caller passing seconds to `Hz::from_raw`
    /// gets a wrong-but-well-typed value. Prefer a named converter where one exists.
    fn from_raw(v: Self::Raw) -> Self;
    /// Unwraps to the raw float, discarding the unit.
    fn to_raw(self) -> Self::Raw;
}

// `From` for the conversions with exactly one answer.
//
// The rule this file follows, stated once so the split is never accidental:
//
// A conversion is `From` when it is total, lossless, and there is only one
// sensible result. It stays a *named method* when any of those fails:
//
//   - It needs another input. `Seconds::to_samples` takes a `SampleRate`,
//     `BeatDuration::to_seconds` takes a `Bpm`. Not two-type conversions.
//   - The answer is not unique — the shape to watch for is a conversion with
//     a policy knob. If two call sites would want different results, a `From`
//     impl has to pick one and apply it silently at every `.into()`.
//
//     The policy belongs to the consumer that holds an opinion about it, not
//     to a default buried in the impl. `Amplitude` → `Db` is the worked
//     example: the silence floor is each caller's to pin, which leaves one
//     answer here (`-inf`) and makes the `From` impl honest.
//   - The target is a bare scalar. `Cents::to_pitch_ratio` and
//     `Semitones::to_pitch_ratio` both land on `f32`; as `From` impls they
//     would be two different meanings of one target type.
//
// The impls below delegate to the inherent methods rather than repeating the
// arithmetic, so there is one implementation and two spellings.

#[cfg(test)]
mod tests;
