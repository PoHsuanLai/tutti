//! Dither — the one per-block processing stage.
//!
//! Dither is the only mastering step that is genuinely streamable: it needs no
//! look-ahead, only a continuous RNG across block boundaries. Resampling is also
//! block-based (rubato), and normalization is the caller's two-pass composition
//! (see `tutti_analysis::loudness`), so this is the whole of what a "mastering"
//! stage would otherwise hold.

use crate::config::ExportConfig;
use crate::options::{BitDepth, Dither};

/// Per-render dither state: the mode, the target depth, and the RNG.
///
/// Carries the RNG so the noise sequence is continuous across blocks, and
/// across channels within a block — a fresh sequence per block would correlate
/// the noise to the block grid.
pub(crate) struct DitherState {
    random_state: u32,
    dither: Dither,
    /// `None` when there is nothing to dither. See [`Self::for_config`].
    lsb: Option<f32>,
}

impl DitherState {
    /// The dither a config calls for.
    ///
    /// **`Float32` never dithers**, and the guard is load-bearing rather than
    /// tidy. Dither exists to decorrelate *quantization* error, and a 32-bit
    /// float output does not quantize — but the LSB expression below is
    /// `1 << (bits - 1)`, which at 32 bits is `1 << 31`: an `i32` overflow to
    /// `-2147483648`, i.e. a *negative* LSB and sign-flipped noise, and a panic
    /// outright in a build with overflow checks on.
    pub(crate) fn for_config(config: &ExportConfig) -> Self {
        let lsb = match (config.dither, config.encode.bit_depth) {
            (Dither::Off, _) | (_, BitDepth::Float32) => None,
            // `bits - 1` is the magnitude of a full-scale integer sample, so
            // its reciprocal is one LSB in the [-1, 1] float domain.
            (_, depth) => Some(1.0 / (1u32 << (depth.bits() - 1)) as f32),
        };
        Self {
            random_state: 0x12345678,
            dither: config.dither,
            lsb,
        }
    }

    /// Dither one block of **interleaved** samples in place. A no-op when
    /// nothing quantizes.
    ///
    /// Flat rather than frame-nested, but the RNG still advances exactly once
    /// per sample in interleaved order — which is what keeps the sequence
    /// "continuous across blocks, and across channels within a block". The
    /// frame width is irrelevant to the noise, so it is not a parameter.
    pub(crate) fn apply(&mut self, samples: &mut [f32]) {
        let Some(lsb) = self.lsb else { return };
        for s in samples.iter_mut() {
            let noise = match self.dither {
                Dither::Off => 0.0,
                Dither::Rectangular => self.rectangular_noise(),
                Dither::Triangular => self.triangular_noise(),
            };
            *s += noise * lsb;
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
    use crate::config::EncodeConfig;

    fn config(dither: Dither, bit_depth: BitDepth) -> ExportConfig {
        ExportConfig {
            encode: EncodeConfig {
                bit_depth,
                ..Default::default()
            },
            dither,
            ..Default::default()
        }
    }

    /// At `Float32` nothing may be added at all — the guard whose absence makes
    /// `1 << 31` overflow to a negative LSB.
    #[test]
    fn float32_is_never_dithered() {
        let mut d = DitherState::for_config(&config(Dither::Triangular, BitDepth::Float32));
        assert!(d.lsb.is_none());
        let mut samples = [0.5f32; 16];
        d.apply(&mut samples);
        assert!(
            samples.iter().all(|&s| s == 0.5),
            "float output must pass through untouched, got {:?}",
            samples[0]
        );
    }

    #[test]
    fn off_is_a_no_op() {
        let mut d = DitherState::for_config(&config(Dither::Off, BitDepth::Int16));
        let mut samples = [0.25f32, -0.25, 0.25, -0.25, 0.25, -0.25, 0.25, -0.25];
        d.apply(&mut samples);
        assert_eq!(
            samples,
            [0.25, -0.25, 0.25, -0.25, 0.25, -0.25, 0.25, -0.25]
        );
    }

    /// The noise is bounded by one LSB at the target depth — audible dither
    /// would be a bug, and a wrong-sign or wrong-scale LSB shows up here.
    #[test]
    fn noise_stays_within_one_lsb() {
        for depth in [BitDepth::Int16, BitDepth::Int24] {
            let mut d = DitherState::for_config(&config(Dither::Triangular, depth));
            let lsb = d.lsb.expect("integer depths dither");
            assert!(lsb > 0.0, "LSB must be positive, got {lsb}");
            let mut samples = [0.0f32; 512];
            d.apply(&mut samples);
            for &s in &samples {
                assert!(s.abs() <= lsb, "|{s}| exceeded one LSB ({lsb})");
            }
        }
    }

    /// The RNG advances once per SAMPLE, and the sequence continues across
    /// calls. Dithering one block of N samples must give exactly what two
    /// blocks of N/2 give — the property that keeps the noise from correlating
    /// to the block grid, and the one a width-aware rewrite could break.
    #[test]
    fn the_noise_sequence_is_continuous_across_blocks() {
        let cfg = config(Dither::Triangular, BitDepth::Int16);

        let mut whole = [0.0f32; 16];
        DitherState::for_config(&cfg).apply(&mut whole);

        let mut split = [0.0f32; 16];
        let mut d = DitherState::for_config(&cfg);
        let (a, b) = split.split_at_mut(6);
        d.apply(a);
        d.apply(b);

        assert_eq!(
            whole, split,
            "a split block must draw the same noise as one whole block"
        );
    }

    /// A 16-bit LSB is 2^-15; 24-bit is 2^-23. Derived from the depth, not read
    /// back from the implementation.
    #[test]
    fn the_lsb_matches_the_bit_depth() {
        let d16 = DitherState::for_config(&config(Dither::Triangular, BitDepth::Int16));
        let d24 = DitherState::for_config(&config(Dither::Triangular, BitDepth::Int24));
        assert_eq!(d16.lsb, Some(1.0 / 32768.0));
        assert_eq!(d24.lsb, Some(1.0 / 8388608.0));
    }
}
