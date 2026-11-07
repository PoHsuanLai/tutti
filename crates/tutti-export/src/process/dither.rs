//! Dither — the one per-block processing stage.
//!
//! Dither is the only mastering step that is genuinely streamable: it needs no
//! look-ahead, only a continuous RNG across block boundaries. Resampling is also
//! block-based (an FFT overlap-add resampler), and normalization is the caller's two-pass composition
//! (see `tutti_dsp::measure_loudness`), so this is the whole of what a "mastering"
//! stage would otherwise hold.

use crate::config::ExportConfig;
use crate::options::Dither;

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
    /// `Float32` never dithers, and the guard is load-bearing rather than
    /// tidy. Dither exists to decorrelate *quantization* error, and a 32-bit
    /// float output does not quantize — but the LSB expression below is
    /// `1 << (bits - 1)`, which at 32 bits is `1 << 31`: an `i32` overflow to
    /// `-2147483648`, i.e. a *negative* LSB and sign-flipped noise, and a panic
    /// outright in a build with overflow checks on.
    ///
    /// Ogg Vorbis never dithers either: its encoder takes the float stream
    /// and quantizes nothing, so the noise would only be encoded as signal.
    /// Both are a format with no integer width.
    pub(crate) fn for_config(config: &ExportConfig) -> Self {
        let lsb = match (config.dither, config.encode.format.int_depth()) {
            (Dither::Off, _) | (_, None) => None,
            // `bits - 1` is the magnitude of a full-scale integer sample, so
            // its reciprocal is one LSB in the [-1, 1] float domain.
            (_, Some(depth)) => Some(1.0 / (1u32 << (depth.bits() - 1)) as f32),
        };
        Self {
            random_state: 0x12345678,
            dither: config.dither,
            lsb,
        }
    }

    /// Whether [`apply`](Self::apply) leaves every sample as it is.
    pub(crate) fn is_off(&self) -> bool {
        self.lsb.is_none()
    }

    /// Dither one block of interleaved samples in place. A no-op when
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
mod tests;
