//! Parameter vocabulary — the three things that describe and carry a unit value.
//!
//! Grouped by what each does:
//! - [`units`] — the [`Unit`](units::Unit) trait + the measurement-unit newtypes
//!   (`Bpm`, `Hz`, `Db`, `Semitones`, …). "What kind of value this is."
//! - [`atomic`] — [`Param`](atomic::Param), a lock-free atomic cell holding a
//!   `Unit` value, shareable with the audio thread (load/store/handle).
//! - [`addressing`] — [`UnitParam`](addressing::UnitParam), uniform id-based
//!   addressing of a node's parameters, with fundsp `Setting` conversion.

pub mod addressing;
pub mod atomic;
pub mod units;

pub use addressing::UnitParam;
pub use atomic::Param;
pub use units::{
    AtomicSamplePosition, Beat, BeatDuration, Bpm, Cents, Db, Degrees, Hz, Linear, Ratio,
    SamplePosition, SampleRate, Seconds, Semitones, Unit,
};
