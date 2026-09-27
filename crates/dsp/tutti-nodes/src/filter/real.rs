//! The float the filters' state and coefficients run in: `f32` or `f64`.
//!
//! [`SvfFilterNode`](super::SvfFilterNode), [`EqBandNode`](super::EqBandNode)
//! and [`LadderFilterNode`](super::LadderFilterNode) are generic over it, so a
//! host picks `f64` state for low cutoffs at high rates (doc 013, decision 2)
//! and `f32` elsewhere. It is exactly the arithmetic those filters use and
//! nothing more. Until design doc 013 Phase 5 it was `tutti-node`'s `Real`, the
//! top of fundsp's `Num`/`Float`/`Real` tower (SIMD lanes included); the tower
//! went with the fork, and this is the part of it a node here named. The
//! transcendental calls go through `libm`, as the tower's did, so a filter
//! renders the same bits it did before.

use core::fmt::Debug;
use core::ops::{Add, AddAssign, Div, Mul, MulAssign, Neg, Sub, SubAssign};

/// A scalar float for filter state and coefficients: `f32` or `f64`.
pub trait Real:
    Copy
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
    /// `x`, widened or kept.
    fn from_f32(x: f32) -> Self;
    /// This value as `f32`, rounded.
    fn to_f32(self) -> f32;
    /// This value as `f64`, widened or kept.
    fn to_f64(self) -> f64;
    /// Absolute value.
    fn abs(self) -> Self;
    /// The larger of the two (the other when one is NaN, as `f32::max`).
    fn max(self, other: Self) -> Self;
    /// The smaller of the two (the other when one is NaN, as `f32::min`).
    fn min(self, other: Self) -> Self;
    /// Tangent.
    fn tan(self) -> Self;
    /// Hyperbolic tangent.
    fn tanh(self) -> Self;
}

impl Real for f32 {
    #[inline(always)]
    fn from_f64(x: f64) -> Self {
        x as f32
    }
    #[inline(always)]
    fn from_f32(x: f32) -> Self {
        x
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
    fn from_f32(x: f32) -> Self {
        f64::from(x)
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
