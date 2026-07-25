//! Typed parameter newtypes shared with `tutti-core`.
//!
//! This file defines [`SampleRate`], a `#[repr(transparent)]` `f64` newtype
//! used at the [`AudioNode`](crate::audionode::AudioNode) and
//! [`AudioUnit`](crate::audiounit::AudioUnit) trait surfaces. It lives here
//! (rather than in `tutti-core::params`) because `tutti-core` already depends
//! on `fundsp-tutti`; the trait that takes a `SampleRate` parameter must
//! therefore see the type in this crate. `tutti-core` re-exports the same
//! type so downstream code keeps importing `tutti_core::params::SampleRate`.
//!
//! `f64`-backed because sample rates routinely exceed `f32`'s integer-precision
//! range (e.g., 192_000) and are used in time arithmetic where precision matters.
//!
//! Zero-cost: `Copy + Send + Sync + 'static`, dyn-trait compatible, and
//! collapses to a raw `f64` after monomorphization.

/// Audio sample rate in Hertz.
#[repr(transparent)]
// `PartialOrd` only, not `Ord`: float-backed, so `NaN` denies totality exactly
// as it does for the raw `f64` — the same rule the `tutti-types` units follow.
#[derive(Copy, Clone, Debug, PartialEq, PartialOrd, Default)]
pub struct SampleRate(pub f64);

impl SampleRate {
    /// Construct from a raw `f64` value.
    #[inline]
    pub const fn new(v: f64) -> Self {
        Self(v)
    }

    /// Unwrap to the raw `f64` value.
    #[inline]
    pub const fn get(self) -> f64 {
        self.0
    }

    // The `unit_*` op macros live in `tutti_types::value::units` and are
    // crate-private on purpose — eight names as generic as `unit_bounded!`
    // would be permanent public API of a published crate. `SampleRate` is the
    // one unit defined outside that module (it must live here; see the module
    // doc), so it pays for that choice with a hand-written copy of exactly the
    // two opt-ins it needs: ordering, above, and bounds, here.
    //
    // Deliberately absent, and the omissions are load-bearing:
    //
    // - `Add` / `Sub` — 44.1 kHz plus 48 kHz is not a sample rate.
    // - `Mul<f64>` / `Div<f64>` — a scaled rate is a *different device rate*;
    //   re-derive it from the device rather than scaling a stale one.
    // - `Div<SampleRate> -> f64` — that quotient is `SrcRatio`, and
    //   `SrcRatio::for_rates` owns the derivation along with its unity
    //   tolerance and its non-positive guard. A bare operator here would let
    //   callers bypass both.

    /// The lower of two rates.
    #[inline]
    pub fn min(self, other: Self) -> Self {
        Self(f64::min(self.0, other.0))
    }

    /// The higher of two rates.
    #[inline]
    pub fn max(self, other: Self) -> Self {
        Self(f64::max(self.0, other.0))
    }

    /// Constrain into `lo..=hi`. Panics if `lo > hi`, matching `f64::clamp`.
    #[inline]
    pub fn clamp(self, lo: Self, hi: Self) -> Self {
        Self(f64::clamp(self.0, lo.0, hi.0))
    }
}

impl From<f64> for SampleRate {
    #[inline]
    fn from(v: f64) -> Self {
        Self(v)
    }
}

impl From<f32> for SampleRate {
    #[inline]
    fn from(v: f32) -> Self {
        Self(v as f64)
    }
}

impl From<SampleRate> for f64 {
    #[inline]
    fn from(v: SampleRate) -> f64 {
        v.0
    }
}

impl core::fmt::Display for SampleRate {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(&self.0, f)
    }
}

// `SampleRate` participates in the shared `Unit` vocabulary. The trait lives in
// `tutti-types` (which this crate already depends on for `latency`), and the
// type lives here — so the `impl` belongs here, next to the definition, rather
// than in `tutti-core` (which would own neither trait nor type — an orphan
// violation). `tutti-core` re-exports both, so consumers are unaffected.
impl tutti_types::Unit for SampleRate {
    type Raw = f64;
    #[inline]
    fn from_raw(v: f64) -> Self {
        Self(v)
    }
    #[inline]
    fn to_raw(self) -> f64 {
        self.0
    }
}

/// Convenience constant for 44.1 kHz (the historic CD-audio rate).
pub const SR_44K1: SampleRate = SampleRate(44_100.0);

/// Convenience constant for 48 kHz (typical pro-audio default).
pub const SR_48K: SampleRate = SampleRate(48_000.0);
