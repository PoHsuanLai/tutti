//! Parameter vocabulary — the three things that describe and carry a unit value.
//!
//! Two of the three now live in [`tutti_types::value`] (pure vocabulary, no
//! engine dependency) and are re-exported here so `tutti_core::param::*` keeps
//! resolving:
//! - the [`Unit`] trait + the measurement newtypes (`Bpm`, `Hz`, `Db`, …) and
//!   [`AtomicSamplePosition`], plus [`Param`] — the lock-free atomic cell.
//!
//! What stays in tutti-core is the fundsp-coupled part:
//! - [`addressing`] — [`UnitParam`](addressing::UnitParam), uniform id-based
//!   addressing of a node's parameters, built on fundsp's `Setting`.
//! - [`SampleRate`] — defined in `fundsp-tutti` (its `AudioUnit` trait surface
//!   takes it) and re-exported here; its `Unit` impl lives in `fundsp-tutti`.

pub mod addressing;

pub use addressing::UnitParam;

// The pure value vocabulary now lives in `tutti-types`. Re-exported so existing
// `tutti_core::param::{Bpm, Param, Unit, …}` imports keep resolving.
pub use tutti_types::value::{
    AtomicSamplePosition, Beat, BeatDuration, Bpm, Cents, Db, Degrees, Hz, Linear, Param, Ratio,
    SamplePosition, Seconds, Semitones, Unit,
};

// `SampleRate` is fundsp's (its trait surfaces take it); re-exported here so
// `tutti_core::param::SampleRate` keeps resolving. Its `Unit` impl is in
// `fundsp-tutti`, next to the definition.
pub use fundsp::params::SampleRate;
