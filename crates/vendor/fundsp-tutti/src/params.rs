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
#[derive(Copy, Clone, Debug, PartialEq, Default)]
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

/// Convenience constant for 44.1 kHz (the historic CD-audio rate).
pub const SR_44K1: SampleRate = SampleRate(44_100.0);

/// Convenience constant for 48 kHz (typical pro-audio default).
pub const SR_48K: SampleRate = SampleRate(48_000.0);
