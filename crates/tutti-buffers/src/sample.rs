//! [`Sample`] and [`Float`]: the storage formats and the arithmetic types.

use crate::I24;
use core::fmt::Debug;
use core::ops::{Add, AddAssign, Div, Mul, MulAssign, Neg, Sub, SubAssign};

mod sealed {
    /// The closed set of formats: only this crate adds one.
    pub trait Sealed: Copy {
        /// The bit width of an integer format, `None` for a float.
        const INT_BITS: Option<u32>;
        /// The value of an integer format, `None` for a float.
        fn to_int(self) -> Option<i32>;
        /// The format's value for an integer already within its range. A float
        /// format never reaches this.
        fn from_int(x: i32) -> Self;
    }
}

/// A storage format: what a file, a device, a plugin port or a wire holds one
/// value of.
///
/// The implementors are `i8`, `i16`, [`I24`], `i32`, `f32` and `f64`; the trait
/// is sealed. A float format holds the signal as it is, nominally within
/// `-1.0..=1.0`. An `n`-bit integer format holds it scaled by `2^(n-1)`, so
/// full scale is `-2^(n-1)` (that is `-1.0`) to `2^(n-1) - 1`.
///
/// # Conversion
///
/// [`convert_to`](Self::convert_to) is the one conversion between any two
/// formats, and every pair follows the same rule:
///
/// - integer to float is `x / 2^(n-1)`; the result is exact in `f64`, and
///   `f32` rounds it to nearest even;
/// - float to integer is `x * 2^(n-1)`, rounded half to even, clipped to
///   `[MIN, MAX]` of the integer; NaN is `0`, and an infinity clips;
/// - float to float is a plain cast;
/// - integer to integer is the composition of the two above, computed in
///   integers with no float hop. Widening shifts left by the width difference
///   and is exact. Narrowing divides by `2^(n-m)`, rounds half to even and
///   clips to the target's range (so the source's `MAX` into a narrower format
///   rounds up out of range and clips to the target's `MAX`).
///
/// The conversion is not the inverse of itself: widening then narrowing, or
/// integer to float to the same integer, returns the original value; float to
/// integer to float is within one least significant bit of the original (half
/// of one for a value in range).
///
/// ```
/// use tutti_buffers::Sample;
///
/// assert_eq!(0.5f32.convert_to::<i16>(), 16384);
/// assert_eq!(16384i16.convert_to::<f32>(), 0.5);
/// assert_eq!(2.0f32.convert_to::<i16>(), i16::MAX); // clips
/// assert_eq!(f32::NAN.convert_to::<i16>(), 0);
/// assert_eq!(0x1234i16.convert_to::<i32>(), 0x1234_0000);
/// ```
pub trait Sample: sealed::Sealed + Debug + PartialEq + Send + Sync + 'static {
    /// The value of silence: zero.
    const SILENCE: Self;

    /// The value as a nominal-unit `f64`: an integer divided by `2^(n-1)`
    /// (exact), a float widened.
    #[must_use]
    fn to_f64(self) -> f64;

    /// The format's value for a nominal-unit `f64`: a float is cast, an integer
    /// is scaled by `2^(n-1)`, rounded half to even and clipped, with NaN as
    /// `0`.
    #[must_use]
    fn from_f64(x: f64) -> Self;

    /// Converts to another format by the rule in the [trait docs](Sample).
    #[inline]
    #[must_use]
    fn convert_to<T: Sample>(self) -> T {
        match (Self::INT_BITS, self.to_int(), T::INT_BITS) {
            (Some(from), Some(x), Some(to)) => T::from_int(rescale(x, from, to)),
            _ => T::from_f64(self.to_f64()),
        }
    }
}

/// The nominal-unit scale of an `n`-bit integer format: `2^(n-1)`.
#[inline]
const fn scale(bits: u32) -> f64 {
    (1u64 << (bits - 1)) as f64
}

/// Float to an `n`-bit integer: scale, round half to even, clip. NaN is `0`.
#[inline]
fn float_to_int(x: f64, bits: u32) -> i32 {
    if x.is_nan() {
        return 0;
    }
    let s = scale(bits);
    let v = libm::rint(x * s);
    if v >= s - 1.0 {
        (s - 1.0) as i32
    } else if v <= -s {
        (-s) as i32
    } else {
        v as i32
    }
}

/// An `n`-bit integer to `m` bits: the exact integer form of scaling by
/// `2^(m-n)`, rounding half to even and clipping to the `m`-bit range.
#[inline]
fn rescale(x: i32, from: u32, to: u32) -> i32 {
    let x = i64::from(x);
    if to >= from {
        return (x << (to - from)) as i32;
    }
    let d = from - to;
    let floor = x >> d;
    let rem = x & ((1 << d) - 1);
    let half = 1i64 << (d - 1);
    let up = rem > half || (rem == half && floor & 1 == 1);
    let q = floor + i64::from(up);
    let max = (1i64 << (to - 1)) - 1;
    q.clamp(-max - 1, max) as i32
}

macro_rules! int_sample {
    ($t:ty, $bits:literal) => {
        impl sealed::Sealed for $t {
            const INT_BITS: Option<u32> = Some($bits);
            #[inline]
            fn to_int(self) -> Option<i32> {
                Some(i32::from(self))
            }
            #[inline]
            fn from_int(x: i32) -> Self {
                x as $t
            }
        }
        impl Sample for $t {
            const SILENCE: Self = 0;
            #[inline]
            fn to_f64(self) -> f64 {
                f64::from(i32::from(self)) / scale($bits)
            }
            #[inline]
            fn from_f64(x: f64) -> Self {
                float_to_int(x, $bits) as $t
            }
        }
    };
}

int_sample!(i8, 8);
int_sample!(i16, 16);
int_sample!(i32, 32);

impl sealed::Sealed for I24 {
    const INT_BITS: Option<u32> = Some(24);
    #[inline]
    fn to_int(self) -> Option<i32> {
        Some(self.get())
    }
    #[inline]
    fn from_int(x: i32) -> Self {
        I24::of(x)
    }
}

impl Sample for I24 {
    const SILENCE: Self = I24::ZERO;
    #[inline]
    fn to_f64(self) -> f64 {
        f64::from(self.get()) / scale(24)
    }
    #[inline]
    fn from_f64(x: f64) -> Self {
        I24::of(float_to_int(x, 24))
    }
}

impl sealed::Sealed for f32 {
    const INT_BITS: Option<u32> = None;
    #[inline]
    fn to_int(self) -> Option<i32> {
        None
    }
    #[inline]
    fn from_int(_: i32) -> Self {
        0.0
    }
}

impl Sample for f32 {
    const SILENCE: Self = 0.0;
    #[inline]
    fn to_f64(self) -> f64 {
        f64::from(self)
    }
    #[inline]
    fn from_f64(x: f64) -> Self {
        x as f32
    }
}

impl sealed::Sealed for f64 {
    const INT_BITS: Option<u32> = None;
    #[inline]
    fn to_int(self) -> Option<i32> {
        None
    }
    #[inline]
    fn from_int(_: i32) -> Self {
        0.0
    }
}

impl Sample for f64 {
    const SILENCE: Self = 0.0;
    #[inline]
    fn to_f64(self) -> f64 {
        self
    }
    #[inline]
    fn from_f64(x: f64) -> Self {
        x
    }
}

/// A [`Sample`] that arithmetic runs in: `f32` or `f64`.
///
/// Its math goes through the `libm` crate, never the standard library's float
/// methods, so a render is the same on every platform.
///
/// ```
/// use tutti_buffers::Float;
///
/// assert_eq!(<f32 as Float>::sqrt(4.0), 2.0);
/// assert_eq!(<f64 as Float>::exp(0.0), 1.0);
/// ```
pub trait Float:
    Sample
    + Copy
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
    /// Zero.
    const ZERO: Self;
    /// One.
    const ONE: Self;
    /// The absolute value.
    #[must_use]
    fn abs(self) -> Self;
    /// The largest integer not above the value.
    #[must_use]
    fn floor(self) -> Self;
    /// The smallest integer not below the value.
    #[must_use]
    fn ceil(self) -> Self;
    /// The square root.
    #[must_use]
    fn sqrt(self) -> Self;
    /// `e` to the value.
    #[must_use]
    fn exp(self) -> Self;
    /// The natural logarithm.
    #[must_use]
    fn ln(self) -> Self;
    /// The value to the power `y`.
    #[must_use]
    fn powf(self, y: Self) -> Self;
    /// The sine, in radians.
    #[must_use]
    fn sin(self) -> Self;
    /// The cosine, in radians.
    #[must_use]
    fn cos(self) -> Self;
}

macro_rules! float_impl {
    ($t:ty, $abs:ident, $floor:ident, $ceil:ident, $sqrt:ident, $exp:ident, $ln:ident, $pow:ident, $sin:ident, $cos:ident) => {
        impl Float for $t {
            const ZERO: Self = 0.0;
            const ONE: Self = 1.0;
            #[inline]
            fn abs(self) -> Self {
                libm::$abs(self)
            }
            #[inline]
            fn floor(self) -> Self {
                libm::$floor(self)
            }
            #[inline]
            fn ceil(self) -> Self {
                libm::$ceil(self)
            }
            #[inline]
            fn sqrt(self) -> Self {
                libm::$sqrt(self)
            }
            #[inline]
            fn exp(self) -> Self {
                libm::$exp(self)
            }
            #[inline]
            fn ln(self) -> Self {
                libm::$ln(self)
            }
            #[inline]
            fn powf(self, y: Self) -> Self {
                libm::$pow(self, y)
            }
            #[inline]
            fn sin(self) -> Self {
                libm::$sin(self)
            }
            #[inline]
            fn cos(self) -> Self {
                libm::$cos(self)
            }
        }
    };
}

float_impl!(
    f32, fabsf, floorf, ceilf, sqrtf, expf, logf, powf, sinf, cosf
);
float_impl!(f64, fabs, floor, ceil, sqrt, exp, log, pow, sin, cos);
