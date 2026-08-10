//! Float→PCM sample quantization, and the depth vocabulary that selects it.
//!
//! The canonical conversion from a normalized `f32` sample to fixed-point PCM,
//! shared by every codec/sink in the engine (the export encoders, the live
//! `WavOut`) so a recorded and an exported file quantize a given sample
//! identically instead of each carrying its own copy.
//!
//! Both round to nearest rather than truncating: bare `as iN` truncation biases
//! every sample toward zero (a consistent negative DC error on the negative
//! half), whereas `.round()` is unbiased.
//!
//! # One depth vocabulary, beside its quantizers
//!
//! [`BitDepth`] lives here rather than in any one sink, so a sink chooses a
//! depth instead of inventing a vocabulary for one, and [`BitDepth::quantize`]
//! is the single dispatch. Two sinks agreeing on what `Int24` means is then
//! structural rather than a coincidence between hand-written `match` arms.
//!
//! # Codec-free on purpose
//!
//! `quantize` returns a [`Sample`] rather than writing anything. This crate is
//! the root leaf — every engine crate depends on it, and each of its own
//! dependencies is `default-features = false` — so a file-format codec must not
//! reach it. Each sink spells its own writer call per variant: three lines it
//! cannot get wrong, over an arithmetic it does not own.

/// Sample width a PCM sink writes.
///
/// `Int24` is the default because that is what a file export wants; a live
/// capture path typically picks `Float32` explicitly (no quantization, no
/// clipping to worry about mid-take).
///
/// **Deliberately not `#[non_exhaustive]`**, unlike most vocabulary in this
/// crate. Encoders match it across a crate boundary, so that attribute would
/// force a `_` arm into each of them — and a `_` arm is exactly what must not
/// exist here: a new depth an encoder silently ignores writes a file whose
/// header and data disagree. This is a closed structural set (the widths a PCM
/// file can hold), so adding one *should* fail to compile everywhere it is
/// handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum BitDepth {
    /// 16-bit signed integer — CD depth.
    Int16,
    /// 24-bit signed integer, packed three bytes per sample. The export default.
    #[default]
    Int24,
    /// 32-bit float, written unquantized — no dither and no clipping decision.
    Float32,
}

/// One sample, quantized to a [`BitDepth`].
///
/// A tagged value rather than bytes: the caller hands the variant to whatever
/// writer it holds, so the arithmetic is shared without this crate knowing any
/// file format. See the [module docs](self) for why that separation is load-bearing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Sample {
    /// 16-bit signed, ready to write little-endian.
    I16(i16),
    /// 24-bit signed, stored in an `i32`. Only the low three bytes are written.
    I24(i32),
    /// 32-bit float, carried through unquantized.
    F32(f32),
}

impl BitDepth {
    /// Bits per sample as written to a file header.
    pub fn bits(&self) -> u16 {
        match self {
            Self::Int16 => 16,
            Self::Int24 => 24,
            Self::Float32 => 32,
        }
    }

    /// Whether this depth quantizes at all. `Float32` passes samples through, so
    /// dithering it is a no-op and clipping is the caller's concern.
    pub fn is_integer(&self) -> bool {
        !matches!(self, Self::Float32)
    }

    /// Quantize one normalized `f32` to this depth.
    ///
    /// The single dispatch every PCM sink shares. Out-of-range input is clamped
    /// by the underlying converters rather than wrapping.
    #[inline]
    pub fn quantize(self, sample: f32) -> Sample {
        match self {
            Self::Int16 => Sample::I16(f32_to_i16(sample)),
            Self::Int24 => Sample::I24(f32_to_i24(sample)),
            Self::Float32 => Sample::F32(sample),
        }
    }
}

/// Quantize a normalized `f32` (`[-1.0, 1.0]`) to signed 16-bit PCM, clamping
/// out-of-range input.
#[inline]
pub fn f32_to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

/// Quantize a normalized `f32` (`[-1.0, 1.0]`) to signed 24-bit PCM (stored in
/// an `i32`), clamping out-of-range input. 24-bit signed range is
/// `[-8_388_608, 8_388_607]`.
#[inline]
pub fn f32_to_i24(sample: f32) -> i32 {
    (sample.clamp(-1.0, 1.0) * 8_388_607.0).round() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f32_to_i16_clamps() {
        assert_eq!(f32_to_i16(0.0), 0);
        assert_eq!(f32_to_i16(1.0), 32767);
        assert_eq!(f32_to_i16(-1.0), -32767);
        assert_eq!(f32_to_i16(1.5), 32767);
        assert_eq!(f32_to_i16(-1.5), -32767);
    }

    #[test]
    fn f32_to_i24_clamps() {
        assert_eq!(f32_to_i24(0.0), 0);
        assert_eq!(f32_to_i24(1.0), 8388607);
        assert_eq!(f32_to_i24(-1.0), -8388607);
        assert_eq!(f32_to_i24(1.5), 8388607);
        assert_eq!(f32_to_i24(-1.5), -8388607);
    }

    /// `quantize` dispatches to the same converters a sink would otherwise call
    /// by hand — the property that makes two sinks agree structurally rather
    /// than by coincidence. Endpoints included, since clamping is where a wrong
    /// arm shows up first.
    #[test]
    fn quantize_agrees_with_the_free_functions_at_every_depth() {
        for &s in &[0.0f32, 0.5, -0.5, 1.0, -1.0, 1.5, -1.5] {
            assert_eq!(
                BitDepth::Int16.quantize(s),
                Sample::I16(f32_to_i16(s)),
                "Int16 must dispatch to f32_to_i16 for {s}"
            );
            assert_eq!(
                BitDepth::Int24.quantize(s),
                Sample::I24(f32_to_i24(s)),
                "Int24 must dispatch to f32_to_i24 for {s}"
            );
            assert_eq!(
                BitDepth::Float32.quantize(s),
                Sample::F32(s),
                "Float32 must pass through unchanged for {s}"
            );
        }
    }

    /// Each depth produces its own variant — so a sink's `match` cannot silently
    /// write 16-bit samples into a 24-bit file.
    #[test]
    fn each_depth_is_distinguishable_at_the_call_site() {
        // 0.5 is representable at every depth, so any difference here is the
        // depth's, not rounding's.
        assert_eq!(BitDepth::Int16.quantize(0.5), Sample::I16(16384));
        assert_eq!(BitDepth::Int24.quantize(0.5), Sample::I24(4194304));
        assert_eq!(BitDepth::Float32.quantize(0.5), Sample::F32(0.5));

        assert_eq!(BitDepth::Int16.bits(), 16);
        assert_eq!(BitDepth::Int24.bits(), 24);
        assert_eq!(BitDepth::Float32.bits(), 32);

        assert!(BitDepth::Int16.is_integer());
        assert!(BitDepth::Int24.is_integer());
        assert!(
            !BitDepth::Float32.is_integer(),
            "float is the one depth that does not quantize"
        );
    }

    /// Every `Sample` variant maps to exactly one depth, so a writer's `match`
    /// on it cannot silently pick the wrong width.
    ///
    /// This is what makes "the live sink and the export encoder agree by
    /// construction" true rather than aspirational: both write
    /// `match depth.quantize(s) { I16 => .., I24 => .., F32 => .. }`, and the
    /// variant they receive is decided here, once. A writer that mismatched an
    /// arm would be writing a width the header never declared.
    #[test]
    fn a_variant_pins_the_width_a_writer_must_use() {
        // Deliberately not a round number: any arm confusion changes the value,
        // not just the type.
        let s = 0.3_f32;
        match BitDepth::Int16.quantize(s) {
            Sample::I16(v) => assert_eq!(v, f32_to_i16(s)),
            other => panic!("Int16 must yield an I16 sample, got {other:?}"),
        }
        match BitDepth::Int24.quantize(s) {
            Sample::I24(v) => assert_eq!(v, f32_to_i24(s)),
            other => panic!("Int24 must yield an I24 sample, got {other:?}"),
        }
        match BitDepth::Float32.quantize(s) {
            Sample::F32(v) => assert_eq!(v, s),
            other => panic!("Float32 must yield an F32 sample, got {other:?}"),
        }
    }
}
