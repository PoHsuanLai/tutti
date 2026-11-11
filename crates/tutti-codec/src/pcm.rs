//! The sample widths a PCM file holds, and the one quantization every encoder
//! shares.
//!
//! [`Pcm::quantize`] turns a normalized `f32` into the fixed-point value a
//! writer stores. Both converters round to nearest rather than truncating: bare
//! `as iN` truncation biases every sample toward zero (a consistent negative DC
//! error on the negative half), whereas `.round()` is unbiased. Every container
//! quantizes through here, so an exported WAV, AIFF and FLAC agree on a sample.
//!
//! ```
//! use tutti_codec::{Pcm, Sample};
//!
//! assert_eq!(Pcm::Int16.quantize(1.0), Sample::I16(32_767));
//! assert_eq!(Pcm::Int24.quantize(-2.0), Sample::I24(-8_388_607)); // clamped
//! assert_eq!(Pcm::Float32.quantize(0.25), Sample::F32(0.25));
//! assert_eq!(Pcm::default().bits(), 24);
//! ```

/// An integer sample width: the depths FLAC holds, and the integer half of
/// [`Pcm`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum IntDepth {
    /// 16-bit signed integer, CD depth.
    Int16,
    /// 24-bit signed integer, packed three bytes per sample. The default.
    #[default]
    Int24,
}

impl IntDepth {
    /// The bits per sample, as written to a file header.
    #[must_use]
    pub const fn bits(self) -> u16 {
        match self {
            Self::Int16 => 16,
            Self::Int24 => 24,
        }
    }

    /// One least significant bit in the `[-1, 1]` float domain: the reciprocal
    /// of a full-scale integer sample, `2^-(bits - 1)`. The size dither noise
    /// is scaled to before this depth quantizes.
    #[must_use]
    pub fn lsb(self) -> tutti_types::Amplitude {
        tutti_types::Amplitude(1.0 / (1u32 << (self.bits() - 1)) as f32)
    }

    /// Quantizes one normalized `f32` to this depth, clamping out-of-range
    /// input.
    #[inline]
    #[must_use]
    pub fn quantize(self, sample: f32) -> i32 {
        match self {
            Self::Int16 => i32::from(f32_to_i16(sample)),
            Self::Int24 => f32_to_i24(sample),
        }
    }
}

/// The sample width a PCM file holds.
///
/// `Int24` is the default because that is what a file export wants; a live
/// capture path typically picks `Float32` explicitly (no quantization, no
/// clipping to worry about mid-take).
///
/// Deliberately not `#[non_exhaustive]`: encoders match it, and a `_` arm is
/// what must not exist there. A new width an encoder silently ignored would
/// write a file whose header and data disagree, so adding one fails to compile
/// everywhere it is handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Pcm {
    /// 16-bit signed integer, CD depth.
    Int16,
    /// 24-bit signed integer, packed three bytes per sample. The default.
    #[default]
    Int24,
    /// 32-bit float, written unquantized: no dither and no clipping decision.
    Float32,
}

impl From<IntDepth> for Pcm {
    fn from(depth: IntDepth) -> Self {
        match depth {
            IntDepth::Int16 => Self::Int16,
            IntDepth::Int24 => Self::Int24,
        }
    }
}

/// One sample, quantized to a [`Pcm`] width.
///
/// A tagged value rather than bytes: the writer matches the variant, so the
/// arithmetic is shared without this module knowing any container.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Sample {
    /// 16-bit signed, ready to write little-endian.
    I16(i16),
    /// 24-bit signed, stored in an `i32`. Only the low three bytes are written.
    I24(i32),
    /// 32-bit float, carried through unquantized.
    F32(f32),
}

impl Pcm {
    /// The bits per sample, as written to a file header.
    #[must_use]
    pub const fn bits(self) -> u16 {
        match self {
            Self::Int16 => 16,
            Self::Int24 => 24,
            Self::Float32 => 32,
        }
    }

    /// The integer width, or `None` for `Float32`, which does not quantize:
    /// dithering it is a no-op and clipping is the caller's concern.
    #[must_use]
    pub const fn int_depth(self) -> Option<IntDepth> {
        match self {
            Self::Int16 => Some(IntDepth::Int16),
            Self::Int24 => Some(IntDepth::Int24),
            Self::Float32 => None,
        }
    }

    /// Quantizes one normalized `f32` to this width. Out-of-range input is
    /// clamped by the converters rather than wrapping.
    #[inline]
    #[must_use]
    pub fn quantize(self, sample: f32) -> Sample {
        match self {
            Self::Int16 => Sample::I16(f32_to_i16(sample)),
            Self::Int24 => Sample::I24(f32_to_i24(sample)),
            Self::Float32 => Sample::F32(sample),
        }
    }
}

/// Quantizes a normalized `f32` (`[-1.0, 1.0]`) to signed 16-bit PCM, clamping
/// out-of-range input.
///
/// Rounds to nearest: `1.0` is `32_767` and `-1.0` is `-32_767`.
#[inline]
pub(crate) fn f32_to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

/// Quantizes a normalized `f32` (`[-1.0, 1.0]`) to signed 24-bit PCM (stored
/// in an `i32`), clamping out-of-range input.
///
/// Rounds to nearest: `1.0` is `8_388_607` and `-1.0` is `-8_388_607`.
#[inline]
pub(crate) fn f32_to_i24(sample: f32) -> i32 {
    (sample.clamp(-1.0, 1.0) * 8_388_607.0).round() as i32
}

#[cfg(test)]
mod tests;
