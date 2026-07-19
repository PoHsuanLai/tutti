#![cfg_attr(docsrs, feature(doc_cfg))]
//! FunDSP is an audio processing and synthesis library.
//!
//! See `README.md` in crate root folder for an overview.
//! For a list of changes, see `CHANGES.md` in the same folder.
//!
//! The central abstractions are located in the `audionode` and `audiounit` modules.
//! The `combinator` module defines the graph operators.
#![cfg_attr(all(not(feature = "std"), not(test)), no_std)]
#![allow(
    clippy::precedence,
    clippy::type_complexity,
    clippy::float_cmp,
    clippy::len_zero,
    double_negations,
    clippy::needless_range_loop,
    clippy::manual_range_contains,
    clippy::too_many_arguments,
    clippy::comparison_chain,
    clippy::unnecessary_cast
)]

use numeric_array::{ArrayLength, NumericArray};
use typenum::{U1, U4, U8};

use core::cmp::PartialEq;
use core::marker::{Send, Sync};
use core::ops::{Add, AddAssign, Div, DivAssign, Mul, MulAssign, Neg, Sub, SubAssign};
use core::ops::{BitAnd, BitOr, BitXor, Not, Shl, Shr};

use wide::{f32x8, f64x4, i32x8, u32x8};

/// Type-level integer. These are notated as `U0`, `U1`...
pub trait Size<T>: ArrayLength + Sync + Send + Clone {}

impl<T, A: ArrayLength + Sync + Send + Clone> Size<T> for A {}

/// Frames are arrays with a static size used to transport audio data
/// between `AudioNode` instances.
pub type Frame<T, Size> = NumericArray<T, Size>;

#[doc(inline)]
pub use params::SampleRate;

/// Default sample rate is 44.1 kHz, in raw `f64` for math sites.
pub const DEFAULT_SR: f64 = 44_100.0;

/// Default sample rate as a typed [`SampleRate`].
pub const DEFAULT_SAMPLE_RATE: SampleRate = SampleRate(DEFAULT_SR);

/// Binary logarithm of maximum buffer size.
pub const MAX_BUFFER_LOG: usize = 6;

/// Maximum buffer size for block processing is 64 samples.
pub const MAX_BUFFER_SIZE: usize = 1 << MAX_BUFFER_LOG;

/// Blocks are explicitly SIMD accelerated. This is the type of a SIMD element
/// containing successive `f32` samples.
pub type F32x = f32x8;

/// The 32-bit unsigned integer SIMD element corresponding to `F32x`.
pub type U32x = u32x8;

/// The 32-bit signed integer SIMD element corresponding to `F32x`.
pub type I32x = i32x8;

/// Right shift for converting from samples to SIMD elements.
pub const SIMD_S: usize = 3;

/// Blocks are explicitly SIMD accelerated. This is the length of a SIMD element in `f32` samples.
pub const SIMD_N: usize = 1 << SIMD_S;

/// "SIMD mask", `SIMD_N` minus one.
pub const SIMD_M: usize = SIMD_N - 1;

/// Left shift for converting from channel number to SIMD index.
pub const SIMD_C: usize = MAX_BUFFER_LOG - SIMD_S;

/// The length of a buffer in SIMD elements.
pub const SIMD_LEN: usize = MAX_BUFFER_SIZE / SIMD_N;

/// The SIMD type for f64 block processing.
pub type F64x = f64x4;

/// The 32-bit signed integer SIMD element used for f64 index arithmetic.
pub type I64x = i32x8;

/// Abstraction over f32/f64 SIMD sample formats for block processing.
///
/// This trait defines the SIMD geometry for a sample format, allowing
/// buffers and audio traits to be generic over f32 and f64 processing.
pub trait Sample: Copy + Send + Sync + 'static {
    /// The SIMD vector type (f32x8 or f64x4).
    type Simd: Float;
    /// The scalar type (f32 or f64).
    type Scalar: Real;

    /// Log2 of SIMD lane count.
    const S: usize;
    /// SIMD lane count (8 for f32, 4 for f64).
    const N: usize;
    /// Lane mask (N - 1).
    const M: usize;
    /// Left shift for converting from channel number to SIMD index (MAX_BUFFER_LOG - S).
    const C: usize;
    /// SIMD elements per 64-sample channel buffer.
    const LEN: usize;

    /// Construct a SIMD vector from a function that produces each lane value.
    fn simd_from_fn(f: impl FnMut(usize) -> Self::Scalar) -> Self::Simd;

    /// The SIMD zero value.
    fn simd_zero() -> Self::Simd;

    /// The scalar zero value.
    fn scalar_zero() -> Self::Scalar;

    /// Get a lane value from a SIMD vector.
    fn get_lane(simd: &Self::Simd, index: usize) -> Self::Scalar;

    /// Set a lane value in a SIMD vector.
    fn set_lane(simd: &mut Self::Simd, index: usize, value: Self::Scalar);
}

/// 32-bit float sample format for SIMD block processing.
#[derive(Copy, Clone, Debug)]
pub struct F32;

impl Sample for F32 {
    type Simd = f32x8;
    type Scalar = f32;

    const S: usize = 3;
    const N: usize = 8;
    const M: usize = 7;
    const C: usize = MAX_BUFFER_LOG - 3; // 3
    const LEN: usize = MAX_BUFFER_SIZE / 8; // 8

    #[inline(always)]
    fn simd_from_fn(f: impl FnMut(usize) -> f32) -> f32x8 {
        f32x8::new(core::array::from_fn::<f32, 8, _>(f))
    }

    #[inline(always)]
    fn simd_zero() -> f32x8 {
        f32x8::ZERO
    }

    #[inline(always)]
    fn scalar_zero() -> f32 {
        0.0
    }

    #[inline(always)]
    fn get_lane(simd: &f32x8, index: usize) -> f32 {
        simd.as_array()[index]
    }

    #[inline(always)]
    fn set_lane(simd: &mut f32x8, index: usize, value: f32) {
        simd.as_mut_array()[index] = value;
    }
}

/// 64-bit float sample format for SIMD block processing.
#[derive(Copy, Clone, Debug)]
pub struct F64;

impl Sample for F64 {
    type Simd = f64x4;
    type Scalar = f64;

    const S: usize = 2;
    const N: usize = 4;
    const M: usize = 3;
    const C: usize = MAX_BUFFER_LOG - 2; // 4
    const LEN: usize = MAX_BUFFER_SIZE / 4; // 16

    #[inline(always)]
    fn simd_from_fn(f: impl FnMut(usize) -> f64) -> f64x4 {
        f64x4::new(core::array::from_fn::<f64, 4, _>(f))
    }

    #[inline(always)]
    fn simd_zero() -> f64x4 {
        f64x4::ZERO
    }

    #[inline(always)]
    fn scalar_zero() -> f64 {
        0.0
    }

    #[inline(always)]
    fn get_lane(simd: &f64x4, index: usize) -> f64 {
        simd.as_array()[index]
    }

    #[inline(always)]
    fn set_lane(simd: &mut f64x4, index: usize, value: f64) {
        simd.as_mut_array()[index] = value;
    }
}

/// Convert amount from samples to (full or partial) SIMD elements.
#[inline(always)]
pub fn simd_items(samples: usize) -> usize {
    (samples + SIMD_M) >> SIMD_S
}

/// Convert amount from samples to full SIMD elements.
#[inline(always)]
pub fn full_simd_items(samples: usize) -> usize {
    samples >> SIMD_S
}

/// Convert amount from samples to (full or partial) SIMD elements for sample format `S`.
#[inline(always)]
pub fn simd_items_s<S: Sample>(samples: usize) -> usize {
    (samples + S::M) >> S::S
}

/// Convert amount from samples to full SIMD elements for sample format `S`.
#[inline(always)]
pub fn full_simd_items_s<S: Sample>(samples: usize) -> usize {
    samples >> S::S
}

/// Number abstraction that is also defined for SIMD items.
pub trait Num:
    Copy
    + Default
    + Send
    + Sync
    + core::fmt::Display
    + Add<Output = Self>
    + Sub<Output = Self>
    + Mul<Output = Self>
    + Div<Output = Self>
    + AddAssign
    + MulAssign
    + SubAssign
    + DivAssign
    + PartialEq
{
    // The number of elements in the number.
    type Size: ArrayLength + Send + Sync;

    fn zero() -> Self;
    fn one() -> Self;
    fn new(x: i64) -> Self;
    fn from_f64(x: f64) -> Self;
    fn from_f32(x: f32) -> Self;
    fn abs(self) -> Self;
    fn signum(self) -> Self;
    // Note that in numerical code we do not want to define min() and max() in terms of comparisons.
    // It is inadvisable in general to link traits like this; Min and Max traits would be preferable.
    fn min(self, other: Self) -> Self;
    fn max(self, other: Self) -> Self;
    fn pow(self, other: Self) -> Self;
    fn floor(self) -> Self;
    fn ceil(self) -> Self;
    fn round(self) -> Self;
}

macro_rules! impl_signed_num {
    ( $($t:ty),* ) => {
    $( impl Num for $t {
        type Size = U1;
        #[inline(always)] fn zero() -> Self { 0 }
        #[inline(always)] fn one() -> Self { 1 }
        #[inline(always)] fn new(x: i64) -> Self { x as Self }
        #[inline(always)] fn from_f64(x: f64) -> Self { x as Self }
        #[inline(always)] fn from_f32(x: f32) -> Self { x as Self }
        #[inline(always)] fn abs(self) -> Self { <$t>::abs(self) }
        #[inline(always)] fn signum(self) -> Self { <$t>::signum(self) }
        #[inline(always)] fn min(self, other: Self) -> Self { core::cmp::min(self, other) }
        #[inline(always)] fn max(self, other: Self) -> Self { core::cmp::max(self, other) }
        #[inline(always)] fn pow(self, other: Self) -> Self { <$t>::pow(self, other as u32) }
        #[inline(always)] fn floor(self) -> Self { self }
        #[inline(always)] fn ceil(self) -> Self { self }
        #[inline(always)] fn round(self) -> Self { self }
    }) *
    }
}
impl_signed_num! { i8, i16, i32, i64, i128, isize }

macro_rules! impl_unsigned_num {
    ( $($t:ty),* ) => {
    $( impl Num for $t {
        type Size = U1;
        #[inline(always)] fn zero() -> Self { 0 }
        #[inline(always)] fn one() -> Self { 1 }
        #[inline(always)] fn new(x: i64) -> Self { x as Self }
        #[inline(always)] fn from_f64(x: f64) -> Self { x as Self }
        #[inline(always)] fn from_f32(x: f32) -> Self { x as Self }
        #[inline(always)] fn abs(self) -> Self { self }
        #[inline(always)] fn signum(self) -> Self { 1 }
        #[inline(always)] fn min(self, other: Self) -> Self { core::cmp::min(self, other) }
        #[inline(always)] fn max(self, other: Self) -> Self { core::cmp::max(self, other) }
        #[inline(always)] fn pow(self, other: Self) -> Self { <$t>::pow(self, other as u32) }
        #[inline(always)] fn floor(self) -> Self { self }
        #[inline(always)] fn ceil(self) -> Self { self }
        #[inline(always)] fn round(self) -> Self { self }
    }) *
    }
}
impl_unsigned_num! { u8, u16, u32, u64, u128, usize }

impl Num for f32 {
    type Size = U1;

    #[inline(always)]
    fn zero() -> Self {
        0.0
    }
    #[inline(always)]
    fn one() -> Self {
        1.0
    }
    #[inline(always)]
    fn new(x: i64) -> Self {
        x as Self
    }
    #[inline(always)]
    fn from_f64(x: f64) -> Self {
        x as Self
    }
    #[inline(always)]
    fn from_f32(x: f32) -> Self {
        x as Self
    }
    #[inline(always)]
    fn abs(self) -> Self {
        libm::fabsf(self)
    }
    #[inline(always)]
    fn signum(self) -> Self {
        libm::copysignf(1.0, self)
    }
    #[inline(always)]
    fn min(self, other: Self) -> Self {
        self.min(other)
    }
    #[inline(always)]
    fn max(self, other: Self) -> Self {
        self.max(other)
    }
    #[inline(always)]
    fn pow(self, other: Self) -> Self {
        libm::powf(self, other)
    }
    #[inline(always)]
    fn floor(self) -> Self {
        libm::floorf(self)
    }
    #[inline(always)]
    fn ceil(self) -> Self {
        libm::ceilf(self)
    }
    #[inline(always)]
    fn round(self) -> Self {
        libm::roundf(self)
    }
}

impl Num for f64 {
    type Size = U1;

    #[inline(always)]
    fn zero() -> Self {
        0.0
    }
    #[inline(always)]
    fn one() -> Self {
        1.0
    }
    #[inline(always)]
    fn new(x: i64) -> Self {
        x as Self
    }
    #[inline(always)]
    fn from_f64(x: f64) -> Self {
        x as Self
    }
    #[inline(always)]
    fn from_f32(x: f32) -> Self {
        x as Self
    }
    #[inline(always)]
    fn abs(self) -> Self {
        libm::fabs(self)
    }
    #[inline(always)]
    fn signum(self) -> Self {
        libm::copysign(1.0, self)
    }
    #[inline(always)]
    fn min(self, other: Self) -> Self {
        self.min(other)
    }
    #[inline(always)]
    fn max(self, other: Self) -> Self {
        self.max(other)
    }
    #[inline(always)]
    fn pow(self, other: Self) -> Self {
        libm::pow(self, other)
    }
    #[inline(always)]
    fn floor(self) -> Self {
        libm::floor(self)
    }
    #[inline(always)]
    fn ceil(self) -> Self {
        libm::ceil(self)
    }
    #[inline(always)]
    fn round(self) -> Self {
        libm::round(self)
    }
}

impl Num for F32x {
    type Size = U8;

    #[inline(always)]
    fn zero() -> Self {
        F32x::ZERO
    }
    #[inline(always)]
    fn one() -> Self {
        F32x::ONE
    }
    #[inline(always)]
    fn new(x: i64) -> Self {
        F32x::splat(x as f32)
    }
    #[inline(always)]
    fn from_f64(x: f64) -> Self {
        F32x::splat(x as f32)
    }
    #[inline(always)]
    fn from_f32(x: f32) -> Self {
        F32x::splat(x)
    }
    #[inline(always)]
    fn abs(self) -> Self {
        self.abs()
    }
    #[inline(always)]
    fn signum(self) -> Self {
        F32x::ONE.copysign(self)
    }
    #[inline(always)]
    fn min(self, other: Self) -> Self {
        self.fast_min(other)
    }
    #[inline(always)]
    fn max(self, other: Self) -> Self {
        self.fast_max(other)
    }
    #[inline(always)]
    fn pow(self, other: Self) -> Self {
        self.pow_f32x8(other)
    }
    #[inline(always)]
    fn floor(self) -> Self {
        (self - 0.4999999).round()
    }
    #[inline(always)]
    fn ceil(self) -> Self {
        (self + 0.4999999).round()
    }
    #[inline(always)]
    fn round(self) -> Self {
        self.round()
    }
}

impl Num for f64x4 {
    type Size = U4;

    #[inline(always)]
    fn zero() -> Self {
        wide::f64x4::ZERO
    }
    #[inline(always)]
    fn one() -> Self {
        wide::f64x4::ONE
    }
    #[inline(always)]
    fn new(x: i64) -> Self {
        wide::f64x4::splat(x as f64)
    }
    #[inline(always)]
    fn from_f64(x: f64) -> Self {
        wide::f64x4::splat(x)
    }
    #[inline(always)]
    fn from_f32(x: f32) -> Self {
        wide::f64x4::splat(x as f64)
    }
    #[inline(always)]
    fn abs(self) -> Self {
        self.abs()
    }
    #[inline(always)]
    fn signum(self) -> Self {
        wide::f64x4::ONE.copysign(self)
    }
    #[inline(always)]
    fn min(self, other: Self) -> Self {
        self.fast_min(other)
    }
    #[inline(always)]
    fn max(self, other: Self) -> Self {
        self.fast_max(other)
    }
    #[inline(always)]
    fn pow(self, other: Self) -> Self {
        self.pow_f64x4(other)
    }
    #[inline(always)]
    fn floor(self) -> Self {
        (self - 0.4999999).round()
    }
    #[inline(always)]
    fn ceil(self) -> Self {
        (self + 0.4999999).round()
    }
    #[inline(always)]
    fn round(self) -> Self {
        self.round()
    }
}

/// Integer abstraction.
pub trait Int:
    Num
    + PartialOrd
    + Not<Output = Self>
    + BitAnd<Output = Self>
    + BitOr<Output = Self>
    + BitXor<Output = Self>
    + Shl<usize, Output = Self>
    + Shr<usize, Output = Self>
{
    fn wrapping_add(self, other: Self) -> Self;
    fn wrapping_sub(self, other: Self) -> Self;
    fn wrapping_mul(self, other: Self) -> Self;
}

macro_rules! impl_int {
    ( $($t:ty),* ) => {
    $( impl Int for $t {
        #[inline(always)] fn wrapping_add(self, other: Self) -> Self { <$t>::wrapping_add(self, other) }
        #[inline(always)] fn wrapping_sub(self, other: Self) -> Self { <$t>::wrapping_sub(self, other) }
        #[inline(always)] fn wrapping_mul(self, other: Self) -> Self { <$t>::wrapping_mul(self, other) }
    }) *
    }
}
impl_int! { i8, i16, i32, i64, i128, isize, u8, u16, u32, u64, u128, usize }

/// Float abstraction that also applies to SIMD items.
pub trait Float: Num + Neg<Output = Self> {
    const PI: Self;
    const TAU: Self;
    const SQRT_2: Self;
    fn from_float<T: Float>(x: T) -> Self;
    fn to_f64(self) -> f64;
    fn to_f32(self) -> f32;
    fn to_i64(self) -> i64;
    fn tan(self) -> Self;
    fn exp(self) -> Self;
    fn sin(self) -> Self;
    fn cos(self) -> Self;
    fn sqrt(self) -> Self;
    fn reduce_add(self) -> f32;
    fn from_frame(frame: &Frame<f32, Self::Size>) -> Self;
    fn to_frame(self) -> Frame<f32, Self::Size>;
    fn set(&mut self, index: usize, value: f32);
    fn get(&self, index: usize) -> f32;
}

impl Float for f32 {
    const PI: Self = core::f32::consts::PI;
    const TAU: Self = core::f32::consts::TAU;
    const SQRT_2: Self = core::f32::consts::SQRT_2;

    #[inline(always)]
    fn from_float<T: Float>(x: T) -> Self {
        x.to_f32()
    }

    #[inline(always)]
    fn to_f64(self) -> f64 {
        self as f64
    }

    #[inline(always)]
    fn to_f32(self) -> f32 {
        self
    }

    #[inline(always)]
    fn to_i64(self) -> i64 {
        self as i64
    }

    #[inline(always)]
    fn tan(self) -> Self {
        libm::tanf(self)
    }

    #[inline(always)]
    fn exp(self) -> Self {
        libm::expf(self)
    }

    #[inline(always)]
    fn sin(self) -> Self {
        libm::sinf(self)
    }

    #[inline(always)]
    fn cos(self) -> Self {
        libm::cosf(self)
    }

    #[inline(always)]
    fn sqrt(self) -> Self {
        libm::sqrtf(self)
    }

    #[inline(always)]
    fn reduce_add(self) -> f32 {
        self
    }

    #[inline(always)]
    fn from_frame(frame: &Frame<f32, Self::Size>) -> Self {
        frame[0]
    }

    #[inline(always)]
    fn to_frame(self) -> Frame<f32, Self::Size> {
        [self].into()
    }

    #[inline(always)]
    fn set(&mut self, _index: usize, value: f32) {
        *self = value;
    }

    #[inline(always)]
    fn get(&self, _index: usize) -> f32 {
        *self
    }
}

impl Float for f64 {
    const PI: Self = core::f64::consts::PI;
    const TAU: Self = core::f64::consts::TAU;
    const SQRT_2: Self = core::f64::consts::SQRT_2;

    #[inline(always)]
    fn from_float<T: Float>(x: T) -> Self {
        x.to_f64()
    }

    #[inline(always)]
    fn to_f64(self) -> f64 {
        self
    }

    #[inline(always)]
    fn to_f32(self) -> f32 {
        self as f32
    }

    #[inline(always)]
    fn to_i64(self) -> i64 {
        self as i64
    }

    #[inline(always)]
    fn tan(self) -> Self {
        libm::tan(self)
    }

    #[inline(always)]
    fn exp(self) -> Self {
        libm::exp(self)
    }

    #[inline(always)]
    fn sin(self) -> Self {
        libm::sin(self)
    }

    #[inline(always)]
    fn cos(self) -> Self {
        libm::cos(self)
    }

    #[inline(always)]
    fn sqrt(self) -> Self {
        libm::sqrt(self)
    }

    #[inline(always)]
    fn reduce_add(self) -> f32 {
        self as f32
    }

    #[inline(always)]
    fn from_frame(frame: &Frame<f32, Self::Size>) -> Self {
        frame[0] as f64
    }

    #[inline(always)]
    fn to_frame(self) -> Frame<f32, Self::Size> {
        [self as f32].into()
    }

    #[inline(always)]
    fn set(&mut self, _index: usize, value: f32) {
        *self = value as f64;
    }

    #[inline(always)]
    fn get(&self, _index: usize) -> f32 {
        *self as f32
    }
}

impl Float for f32x8 {
    const PI: Self = f32x8::PI;
    const TAU: Self = f32x8::TAU;
    const SQRT_2: Self = f32x8::SQRT_2;

    #[inline(always)]
    fn from_float<T: Float>(x: T) -> Self {
        Self::splat(x.to_f32())
    }

    #[inline(always)]
    fn to_f64(self) -> f64 {
        0.0
    }

    #[inline(always)]
    fn to_f32(self) -> f32 {
        0.0
    }

    #[inline(always)]
    fn to_i64(self) -> i64 {
        0
    }

    #[inline(always)]
    fn tan(self) -> Self {
        f32x8::tan(self)
    }

    #[inline(always)]
    fn exp(self) -> Self {
        f32x8::exp(self)
    }

    #[inline(always)]
    fn sin(self) -> Self {
        f32x8::sin(self)
    }

    #[inline(always)]
    fn cos(self) -> Self {
        f32x8::cos(self)
    }

    #[inline(always)]
    fn sqrt(self) -> Self {
        f32x8::sqrt(self)
    }

    #[inline(always)]
    fn reduce_add(self) -> f32 {
        f32x8::reduce_add(self)
    }

    #[inline(always)]
    fn from_frame(frame: &Frame<f32, U8>) -> Self {
        f32x8::new((*frame.as_array()).into())
    }

    #[inline(always)]
    fn to_frame(self) -> Frame<f32, U8> {
        f32x8::to_array(self).into()
    }

    #[inline(always)]
    fn set(&mut self, index: usize, value: f32) {
        self.as_mut_array()[index] = value;
    }

    #[inline(always)]
    fn get(&self, index: usize) -> f32 {
        self.as_array()[index]
    }
}

impl Float for f64x4 {
    const PI: Self = f64x4::PI;
    const TAU: Self = f64x4::TAU;
    const SQRT_2: Self = f64x4::SQRT_2;

    #[inline(always)]
    fn from_float<T: Float>(x: T) -> Self {
        Self::splat(x.to_f64())
    }

    #[inline(always)]
    fn to_f64(self) -> f64 {
        0.0
    }

    #[inline(always)]
    fn to_f32(self) -> f32 {
        0.0
    }

    #[inline(always)]
    fn to_i64(self) -> i64 {
        0
    }

    #[inline(always)]
    fn tan(self) -> Self {
        f64x4::tan(self)
    }

    #[inline(always)]
    fn exp(self) -> Self {
        f64x4::exp(self)
    }

    #[inline(always)]
    fn sin(self) -> Self {
        f64x4::sin(self)
    }

    #[inline(always)]
    fn cos(self) -> Self {
        f64x4::cos(self)
    }

    #[inline(always)]
    fn sqrt(self) -> Self {
        f64x4::sqrt(self)
    }

    #[inline(always)]
    fn reduce_add(self) -> f32 {
        f64x4::reduce_add(self) as f32
    }

    #[inline(always)]
    fn from_frame(frame: &Frame<f32, U4>) -> Self {
        let array = frame.as_array();
        let f64_array = [
            f64::from(array[0]),
            f64::from(array[1]),
            f64::from(array[2]),
            f64::from(array[3]),
        ];
        f64x4::new(f64_array)
    }

    #[inline(always)]
    fn to_frame(self) -> Frame<f32, U4> {
        let array_f64: [f64; 4] = f64x4::to_array(self);
        let array_f32: [f32; 4] = array_f64.map(|x| x as f32);
        array_f32.into()
    }

    #[inline(always)]
    fn set(&mut self, index: usize, value: f32) {
        self.as_mut_array()[index] = value as f64;
    }

    #[inline(always)]
    fn get(&self, index: usize) -> f32 {
        self.as_array()[index] as f32
    }
}

/// Generic floating point conversion function.
#[inline(always)]
pub fn convert<T: Float, U: Float>(x: T) -> U {
    U::from_float(x)
}

/// Refined float abstraction for scalars.
pub trait Real: Float + PartialOrd {
    fn exp2(self) -> Self;
    fn log(self) -> Self;
    fn log2(self) -> Self;
    fn log10(self) -> Self;
    fn tanh(self) -> Self;
    fn atan(self) -> Self;
}

impl Real for f32 {
    #[inline(always)]
    fn exp2(self) -> Self {
        libm::exp2f(self)
    }
    #[inline(always)]
    fn log(self) -> Self {
        libm::logf(self)
    }
    #[inline(always)]
    fn log2(self) -> Self {
        libm::log2f(self)
    }
    #[inline(always)]
    fn log10(self) -> Self {
        libm::log10f(self)
    }
    #[inline(always)]
    fn tanh(self) -> Self {
        libm::tanhf(self)
    }
    #[inline(always)]
    fn atan(self) -> Self {
        libm::atanf(self)
    }
}

impl Real for f64 {
    #[inline(always)]
    fn exp2(self) -> Self {
        libm::exp2(self)
    }
    #[inline(always)]
    fn log(self) -> Self {
        libm::log(self)
    }
    #[inline(always)]
    fn log2(self) -> Self {
        libm::log2(self)
    }
    #[inline(always)]
    fn log10(self) -> Self {
        libm::log10(self)
    }
    #[inline(always)]
    fn tanh(self) -> Self {
        libm::tanh(self)
    }
    #[inline(always)]
    fn atan(self) -> Self {
        libm::atan(self)
    }
}

pub mod adsr;
pub mod audionode;
pub mod audiounit;
pub mod biquad;
pub mod biquad_bank;
pub mod buffer;
pub mod combinator;
pub mod delay;
pub mod denormal;
pub mod dynamics;
pub mod envelope;
pub mod feedback;
pub mod fft;
pub mod filter;
pub mod fir;
pub mod follow;
pub mod generate;
pub mod granular;
pub mod graph;
pub mod math;
pub mod moog;
pub mod net;
pub mod noise;
pub mod oscillator;
pub mod oversample;
pub mod pan;
pub mod params;
pub mod peak_builder;
pub mod prelude;
pub mod prelude32;
pub mod prelude64;
pub mod realnet;
pub mod realseq;
pub mod resample;
pub mod resynth;
pub mod reverb;
pub mod rez;
pub mod ring;
pub mod sequencer;
pub mod setting;
pub mod shape;
pub mod shared;
pub mod signal;
pub mod slot;
pub mod snoop;
pub mod sound;
pub mod svf;
pub mod system;
pub mod vertex;
pub mod wave;
pub mod wavetable;

// GenericSequence is for Frame::generate.
pub use numeric_array::{
    self,
    generic_array::sequence::{Concat, GenericSequence},
    typenum,
};

pub use funutd;
pub use lfqueue;
pub use wide;

extern crate alloc;
pub use alloc::sync::Arc;
pub type Queue<T, const N: usize> = lfqueue::ConstBoundedQueue<T, N>;

#[cfg(feature = "std")]
pub mod write;

#[cfg(all(
    feature = "std",
    any(
        feature = "wav",
        feature = "flac",
        feature = "mp3",
        feature = "ogg",
        feature = "files"
    )
))]
pub mod read;

#[cfg(all(feature = "std", feature = "fft"))]
pub mod convolve;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f32_sample_constants() {
        assert_eq!(F32::S, 3);
        assert_eq!(F32::N, 8);
        assert_eq!(F32::M, 7);
        assert_eq!(F32::C, 3);
        assert_eq!(F32::LEN, 8);
        // LEN * N == MAX_BUFFER_SIZE
        assert_eq!(F32::LEN * F32::N, MAX_BUFFER_SIZE);
    }

    #[test]
    fn f64_sample_constants() {
        assert_eq!(F64::S, 2);
        assert_eq!(F64::N, 4);
        assert_eq!(F64::M, 3);
        assert_eq!(F64::C, 4);
        assert_eq!(F64::LEN, 16);
        // LEN * N == MAX_BUFFER_SIZE
        assert_eq!(F64::LEN * F64::N, MAX_BUFFER_SIZE);
    }

    #[test]
    fn f32_backward_compat_constants() {
        // Existing global constants must match F32 values.
        assert_eq!(SIMD_S, F32::S);
        assert_eq!(SIMD_N, F32::N);
        assert_eq!(SIMD_M, F32::M);
        assert_eq!(SIMD_C, F32::C);
        assert_eq!(SIMD_LEN, F32::LEN);
    }

    #[test]
    fn simd_from_fn_f32() {
        let v = F32::simd_from_fn(|i| (i + 1) as f32);
        for i in 0..8 {
            assert_eq!(F32::get_lane(&v, i), (i + 1) as f32);
        }
    }

    #[test]
    fn simd_from_fn_f64() {
        let v = F64::simd_from_fn(|i| (i + 10) as f64);
        for i in 0..4 {
            assert_eq!(F64::get_lane(&v, i), (i + 10) as f64);
        }
    }

    #[test]
    fn simd_set_get_lane_f32() {
        let mut v = F32::simd_zero();
        F32::set_lane(&mut v, 3, 42.0);
        assert_eq!(F32::get_lane(&v, 3), 42.0);
        assert_eq!(F32::get_lane(&v, 0), 0.0);
    }

    #[test]
    fn simd_set_get_lane_f64() {
        let mut v = F64::simd_zero();
        F64::set_lane(&mut v, 2, 99.5);
        assert_eq!(F64::get_lane(&v, 2), 99.5);
        assert_eq!(F64::get_lane(&v, 0), 0.0);
    }

    #[test]
    fn simd_items_s_matches_legacy() {
        for samples in 0..=MAX_BUFFER_SIZE {
            assert_eq!(simd_items_s::<F32>(samples), simd_items(samples));
            assert_eq!(full_simd_items_s::<F32>(samples), full_simd_items(samples));
        }
    }

    #[test]
    fn simd_items_s_f64() {
        // 0 samples => 0 SIMD items
        assert_eq!(simd_items_s::<F64>(0), 0);
        // 1..4 samples => 1 SIMD item (partial)
        assert_eq!(simd_items_s::<F64>(1), 1);
        assert_eq!(simd_items_s::<F64>(4), 1);
        // 5 samples => 2 SIMD items
        assert_eq!(simd_items_s::<F64>(5), 2);
        // 64 samples => 16 SIMD items
        assert_eq!(simd_items_s::<F64>(64), 16);
        // Full items: 3 samples => 0 full items
        assert_eq!(full_simd_items_s::<F64>(3), 0);
        // Full items: 4 samples => 1 full item
        assert_eq!(full_simd_items_s::<F64>(4), 1);
    }
}
