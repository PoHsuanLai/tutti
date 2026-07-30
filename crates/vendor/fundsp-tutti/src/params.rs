//! Typed parameter newtypes shared with `tutti-core`.
//!
//! [`SampleRate`] is the type the [`AudioNode`](crate::audionode::AudioNode)
//! and [`AudioUnit`](crate::audiounit::AudioUnit) trait surfaces take. It is
//! **defined in `tutti-types`**, alongside the rest of the unit vocabulary, and
//! merely re-exported here.
//!
//! It used to be defined in this file, on the grounds that a trait taking the
//! parameter has to see the type and `tutti-core` already depends on
//! `fundsp-tutti`. That reasoning skipped a step: `fundsp-tutti` depends on
//! `tutti-types`, so the unit crate is *upstream* of this one and can host the
//! definition perfectly well. Moving it there ended an exception —
//! `SampleRate` was the one unit outside `tutti_types::value::units`, and it
//! paid for that with hand-written copies of the `unit_ordered!` /
//! `unit_bounded!` opt-ins and its own `Unit` impl. It now gets those from the
//! same macros as every other unit, and its omission ledger sits with the rest.
//!
//! The re-export stays so `fundsp`-internal code and downstream crates keep
//! importing `fundsp::params::SampleRate` (and `tutti_core::params::SampleRate`
//! through the re-export chain) unchanged.

pub use tutti_types::value::units::SampleRate;

/// Convenience constant for 44.1 kHz (the historic CD-audio rate).
pub const SR_44K1: SampleRate = SampleRate::SR_44K1;

/// Convenience constant for 48 kHz (typical pro-audio default).
pub const SR_48K: SampleRate = SampleRate::SR_48K;
