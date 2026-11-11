//! The parameters of the laws that take one: [`Exponent`] and [`Curvature`].
//!
//! Each refuses what its law cannot evaluate instead of falling back. A bare
//! `f32` in `Law::PowerIn(-2.0)` used to render a linear ramp, with nothing to
//! say the `-2.0` was never read.

use std::fmt;

/// The raw a checked parameter refused.
macro_rules! refusal {
    ($(#[$meta:meta])* $name:ident, $what:literal) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq)]
        pub struct $name(pub f32);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{} is not {}", self.0, $what)
            }
        }

        impl std::error::Error for $name {}
    };
}

refusal!(
    /// The raw [`Exponent::try_from`] refused.
    ExponentError,
    "a finite exponent above 0"
);
refusal!(
    /// The raw [`Curvature::try_from`] refused.
    CurvatureError,
    "a finite curvature within -64..=64"
);

/// The exponent `p` of a power law (`t^p`): finite and above 0.
///
/// `new` refuses anything else (NaN, infinity, zero, negatives) and never
/// clamps. `2.0` is a quadratic, `0.5` a square root.
///
/// ```
/// use tutti_curve::{Exponent, Law};
/// assert_eq!(Law::PowerIn(Exponent::of(2.0)).eval(0.5), 0.25);
/// assert!(Exponent::new(f32::NAN).is_none());
/// assert!(Exponent::new(0.0).is_none());
/// assert!(Exponent::new(-2.0).is_none());
/// ```
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct Exponent(f32);

impl Exponent {
    /// `1`: the power law that is linear.
    pub const ONE: Self = Self(1.0);

    /// `p`, or `None` unless it is finite and above 0.
    #[must_use]
    pub const fn new(p: f32) -> Option<Self> {
        if p.is_finite() && p > 0.0 {
            Some(Self(p))
        } else {
            None
        }
    }

    /// `p`, for a literal or a constant.
    ///
    /// # Panics
    ///
    /// When `p` is not finite and above 0.
    #[must_use]
    pub const fn of(p: f32) -> Self {
        match Self::new(p) {
            Some(e) => e,
            None => panic!("an exponent is finite and above 0"),
        }
    }

    /// The exponent as a raw float.
    #[must_use]
    pub const fn get(self) -> f32 {
        self.0
    }
}

impl TryFrom<f32> for Exponent {
    type Error = ExponentError;
    fn try_from(p: f32) -> Result<Self, ExponentError> {
        Self::new(p).ok_or(ExponentError(p))
    }
}

impl From<Exponent> for f32 {
    fn from(e: Exponent) -> f32 {
        e.0
    }
}

/// The curvature `k` of [`Law::Exp`](crate::Law::Exp): finite, `|k| <= 64`
/// (`e^64` is far inside `f32`, so the ratio never overflows).
///
/// `new` refuses anything else and never clamps. `k > 0` starts slow, `k < 0`
/// starts fast, `0` is linear.
///
/// ```
/// use tutti_curve::{Curvature, Law};
/// assert!(Law::Exp(Curvature::of(3.0)).eval(0.5) < 0.5);
/// assert!(Curvature::new(100.0).is_none());
/// assert!(Curvature::new(f32::INFINITY).is_none());
/// ```
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct Curvature(f32);

impl Curvature {
    /// The largest `|k|`.
    pub const MAX: f32 = 64.0;
    /// `0`: linear.
    pub const ZERO: Self = Self(0.0);

    /// `k`, or `None` unless it is finite with `|k| <= 64`.
    #[must_use]
    pub const fn new(k: f32) -> Option<Self> {
        if k.is_finite() && k >= -Self::MAX && k <= Self::MAX {
            Some(Self(k))
        } else {
            None
        }
    }

    /// `k`, for a literal or a constant.
    ///
    /// # Panics
    ///
    /// When `k` is not finite or `|k|` is past 64.
    #[must_use]
    pub const fn of(k: f32) -> Self {
        match Self::new(k) {
            Some(c) => c,
            None => panic!("a curvature is finite with |k| <= 64"),
        }
    }

    /// The curvature as a raw float.
    #[must_use]
    pub const fn get(self) -> f32 {
        self.0
    }
}

impl TryFrom<f32> for Curvature {
    type Error = CurvatureError;
    fn try_from(k: f32) -> Result<Self, CurvatureError> {
        Self::new(k).ok_or(CurvatureError(k))
    }
}

impl From<Curvature> for f32 {
    fn from(c: Curvature) -> f32 {
        c.0
    }
}

#[cfg(test)]
mod tests;
