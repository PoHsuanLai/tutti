//! The float the filters' state and coefficients run in: `f32` or `f64`.
//!
//! The `tutti-nodes` filter nodes (`SvfFilterNode`, `EqBandNode` and
//! `LadderFilterNode`) are generic over it, so a
//! host picks `f64` state for low cutoffs at high rates, where an `f32`
//! integrator loses resolution, and `f32` elsewhere. It is exactly the
//! arithmetic those filters use and nothing more. The transcendental calls go
//! through `libm`, so results do not depend on the platform's C math
//! library.

use core::fmt::Debug;
use core::ops::{Add, AddAssign, Div, Mul, MulAssign, Neg, Sub, SubAssign};

/// The arithmetic one frame of a filter recurrence runs in: one value (`f32`,
/// `f64`) or several side by side (`wide::f32x8`, the polysynth's voice lanes).
///
/// The SVF and ladder steps ([`svf_step`](crate::svf_step),
/// [`ladder_step`](crate::ladder_step)) are written once over it, so a lane bank
/// runs the same recurrence as a filter node rather than a copy of it.
///
/// Sealed: the three types above are the set, so `wide` is not a version a
/// downstream crate can plug into. [`Real`] extends it, so it is closed too.
pub trait Lanes:
    sealed::Sealed
    + Copy
    + Add<Output = Self>
    + Sub<Output = Self>
    + Mul<Output = Self>
    + Div<Output = Self>
{
    /// `x` in every lane (for a scalar, `x` widened or kept).
    fn from_f32(x: f32) -> Self;
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for f32 {}
    impl Sealed for f64 {}
    impl Sealed for wide::f32x8 {}
}

impl Lanes for f32 {
    #[inline(always)]
    fn from_f32(x: f32) -> Self {
        x
    }
}

impl Lanes for f64 {
    #[inline(always)]
    fn from_f32(x: f32) -> Self {
        f64::from(x)
    }
}

impl Lanes for wide::f32x8 {
    #[inline(always)]
    fn from_f32(x: f32) -> Self {
        wide::f32x8::splat(x)
    }
}

/// A scalar float for filter state and coefficients, implemented for `f32`
/// and `f64`.
///
/// The filter nodes are generic over it: `SvfFilterNode::<f64>` keeps its
/// state in double precision, which matters for low cutoffs at high sample
/// rates. It is sealed (through [`Lanes`]): `f32` and `f64` are the set.
pub trait Real:
    Lanes
    + Copy
    + Default
    + Debug
    + Send
    + Sync
    + 'static
    + PartialEq
    + PartialOrd
    + Add<Output = Self>
    + Sub<Output = Self>
    + Mul<Output = Self>
    + Div<Output = Self>
    + Neg<Output = Self>
    + AddAssign
    + SubAssign
    + MulAssign
{
    /// `x`, rounded to this width.
    fn from_f64(x: f64) -> Self;
    /// This value as `f32`, rounded.
    fn to_f32(self) -> f32;
    /// This value as `f64`, widened or kept.
    fn to_f64(self) -> f64;
    /// Absolute value.
    #[must_use]
    fn abs(self) -> Self;
    /// The larger of the two (the other when one is NaN, as `f32::max`).
    #[must_use]
    fn max(self, other: Self) -> Self;
    /// The smaller of the two (the other when one is NaN, as `f32::min`).
    #[must_use]
    fn min(self, other: Self) -> Self;
    /// Tangent.
    #[must_use]
    fn tan(self) -> Self;
    /// Hyperbolic tangent.
    #[must_use]
    fn tanh(self) -> Self;
}

impl Real for f32 {
    #[inline(always)]
    fn from_f64(x: f64) -> Self {
        x as f32
    }
    #[inline(always)]
    fn to_f32(self) -> f32 {
        self
    }
    #[inline(always)]
    fn to_f64(self) -> f64 {
        f64::from(self)
    }
    #[inline(always)]
    fn abs(self) -> Self {
        libm::fabsf(self)
    }
    #[inline(always)]
    fn max(self, other: Self) -> Self {
        f32::max(self, other)
    }
    #[inline(always)]
    fn min(self, other: Self) -> Self {
        f32::min(self, other)
    }
    #[inline(always)]
    fn tan(self) -> Self {
        libm::tanf(self)
    }
    #[inline(always)]
    fn tanh(self) -> Self {
        libm::tanhf(self)
    }
}

impl Real for f64 {
    #[inline(always)]
    fn from_f64(x: f64) -> Self {
        x
    }
    #[inline(always)]
    fn to_f32(self) -> f32 {
        self as f32
    }
    #[inline(always)]
    fn to_f64(self) -> f64 {
        self
    }
    #[inline(always)]
    fn abs(self) -> Self {
        libm::fabs(self)
    }
    #[inline(always)]
    fn max(self, other: Self) -> Self {
        f64::max(self, other)
    }
    #[inline(always)]
    fn min(self, other: Self) -> Self {
        f64::min(self, other)
    }
    #[inline(always)]
    fn tan(self) -> Self {
        libm::tan(self)
    }
    #[inline(always)]
    fn tanh(self) -> Self {
        libm::tanh(self)
    }
}
