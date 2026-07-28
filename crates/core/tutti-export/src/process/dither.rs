//! Dither — the one per-block processing stage.
//!
//! Dither is the only mastering step that is genuinely streamable: it needs no
//! look-ahead, only a continuous RNG across block boundaries. Resampling is also
//! block-based (rubato), and normalization is the caller's two-pass composition
//! (see `tutti_analysis::loudness`), so this is all that remains of what used to
//! be a "mastering" stage.

use crate::options::{BitDepth, Dither};
use crate::spec::ExportSpec;

/// Per-render dither state: the mode, the target depth, and the RNG.
///
/// Carries the RNG so the noise sequence is continuous across blocks, and
/// across channels within a block — a fresh sequence per block would correlate
/// the noise to the block grid.
pub(crate) struct DitherState {
    random_state: u32,
    dither: Dither,
    /// `None` when there is nothing to dither. See [`Self::for_spec`].
    lsb: Option<f32>,
}

impl DitherState {
    /// The dither a spec calls for.
    ///
    /// **`Float32` never dithers.** Dither exists to decorrelate *quantization*
    /// error, and a 32-bit float output does not quantize. The previous version
    /// applied it anyway and computed its step as `1 << (bits - 1)` — at 32 bits
    /// that is `1 << 31`, which overflows `i32` to `-2147483648` and produced a
    /// *negative* LSB, i.e. sign-flipped noise. Inaudible at ~4.7e-10, and it
    /// would have panicked in a build with overflow checks on.
    pub(crate) fn for_spec(spec: &ExportSpec) -> Self {
        let lsb = match (spec.dither, spec.encode.bit_depth) {
            (Dither::Off, _) | (_, BitDepth::Float32) => None,
            // `bits - 1` is the magnitude of a full-scale integer sample, so
            // its reciprocal is one LSB in the [-1, 1] float domain.
            (_, depth) => Some(1.0 / (1u32 << (depth.bits() - 1)) as f32),
        };
        Self {
            random_state: 0x12345678,
            dither: spec.dither,
            lsb,
        }
    }

    /// Dither one block of frames in place. A no-op when nothing quantizes.
    pub(crate) fn apply<const CH: usize>(&mut self, frames: &mut [[f32; CH]]) {
        let Some(lsb) = self.lsb else { return };
        for frame in frames.iter_mut() {
            for s in frame.iter_mut() {
                let noise = match self.dither {
                    Dither::Off => 0.0,
                    Dither::Rectangular => self.rectangular_noise(),
                    Dither::Triangular => self.triangular_noise(),
                };
                *s += noise * lsb;
            }
        }
    }

    #[inline]
    fn random(&mut self) -> u32 {
        let mut x = self.random_state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.random_state = x;
        x
    }

    #[inline]
    fn rectangular_noise(&mut self) -> f32 {
        (self.random() as f32 / u32::MAX as f32) - 0.5
    }

    /// Two rectangular draws summed — TPDF, which decorrelates the noise from
    /// the signal in a way a single draw does not.
    #[inline]
    fn triangular_noise(&mut self) -> f32 {
        let r1 = self.random() as f32 / u32::MAX as f32;
        let r2 = self.random() as f32 / u32::MAX as f32;
        r1 - r2
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::EncodeSpec;

    fn spec(dither: Dither, bit_depth: BitDepth) -> ExportSpec {
        ExportSpec {
            encode: EncodeSpec {
                bit_depth,
                ..Default::default()
            },
            dither,
            ..Default::default()
        }
    }

    /// The regression: at `Float32`, `1 << 31` overflows to a negative LSB.
    /// Nothing should be added at all.
    #[test]
    fn float32_is_never_dithered() {
        let mut d = DitherState::for_spec(&spec(Dither::Triangular, BitDepth::Float32));
        assert!(d.lsb.is_none());
        let mut frames = [[0.5f32, 0.5]; 8];
        d.apply(&mut frames);
        assert!(
            frames.iter().all(|f| f == &[0.5, 0.5]),
            "float output must pass through untouched, got {:?}",
            frames[0]
        );
    }

    #[test]
    fn off_is_a_no_op() {
        let mut d = DitherState::for_spec(&spec(Dither::Off, BitDepth::Int16));
        let mut frames = [[0.25f32, -0.25]; 4];
        d.apply(&mut frames);
        assert!(frames.iter().all(|f| f == &[0.25, -0.25]));
    }

    /// The noise is bounded by one LSB at the target depth — audible dither
    /// would be a bug, and a wrong-sign or wrong-scale LSB shows up here.
    #[test]
    fn noise_stays_within_one_lsb() {
        for depth in [BitDepth::Int16, BitDepth::Int24] {
            let mut d = DitherState::for_spec(&spec(Dither::Triangular, depth));
            let lsb = d.lsb.expect("integer depths dither");
            assert!(lsb > 0.0, "LSB must be positive, got {lsb}");
            let mut frames = [[0.0f32; 2]; 256];
            d.apply(&mut frames);
            for f in &frames {
                for &s in f {
                    assert!(s.abs() <= lsb, "|{s}| exceeded one LSB ({lsb})");
                }
            }
        }
    }

    /// A 16-bit LSB is 2^-15; 24-bit is 2^-23. Derived from the depth, not read
    /// back from the implementation.
    #[test]
    fn the_lsb_matches_the_bit_depth() {
        let d16 = DitherState::for_spec(&spec(Dither::Triangular, BitDepth::Int16));
        let d24 = DitherState::for_spec(&spec(Dither::Triangular, BitDepth::Int24));
        assert_eq!(d16.lsb, Some(1.0 / 32768.0));
        assert_eq!(d24.lsb, Some(1.0 / 8388608.0));
    }
}
