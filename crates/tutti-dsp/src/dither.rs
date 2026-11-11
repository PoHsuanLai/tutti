//! Dither noise: the one per-block step of quantizing a float signal.
//!
//! Dither decorrelates quantization error, and it needs no look-ahead, only an
//! RNG that continues across blocks. [`DitherNoise`] is that RNG and the two
//! noise shapes; which signals to dither, and at what depth, is the caller's.

pub(crate) mod sink;

use tutti_types::Amplitude;

/// The probability density of the noise a [`DitherNoise`] adds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DitherShape {
    /// One uniform draw, in `[-0.5, 0.5]` LSB.
    Rectangular,
    /// Two uniform draws subtracted (TPDF), in `[-1, 1]` LSB, which
    /// decorrelates the noise from the signal in a way a single draw does not.
    Triangular,
}

/// A xorshift32 noise source that carries its state across blocks.
///
/// The RNG advances once per sample, in the order the samples are handed over,
/// so the sequence is continuous across blocks and, for interleaved audio,
/// across channels within a block. A fresh sequence per block would correlate
/// the noise to the block grid.
#[derive(Debug, Clone)]
pub struct DitherNoise {
    state: u32,
}

impl DitherNoise {
    /// A source at the seed every export starts from.
    #[must_use]
    pub const fn new() -> Self {
        Self { state: 0x1234_5678 }
    }

    /// Add noise of `shape`, scaled to `lsb` (one least significant bit of the
    /// target depth in the `[-1, 1]` float domain), to every sample in place.
    pub fn apply(&mut self, shape: DitherShape, lsb: Amplitude, samples: &mut [f32]) {
        let lsb = lsb.0;
        for s in samples.iter_mut() {
            let noise = match shape {
                DitherShape::Rectangular => self.rectangular(),
                DitherShape::Triangular => self.triangular(),
            };
            *s += noise * lsb;
        }
    }

    #[inline]
    fn random(&mut self) -> u32 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.state = x;
        x
    }

    #[inline]
    fn rectangular(&mut self) -> f32 {
        (self.random() as f32 / u32::MAX as f32) - 0.5
    }

    #[inline]
    fn triangular(&mut self) -> f32 {
        let r1 = self.random() as f32 / u32::MAX as f32;
        let r2 = self.random() as f32 / u32::MAX as f32;
        r1 - r2
    }
}

impl Default for DitherNoise {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LSB: Amplitude = Amplitude(1.0);

    /// The first draws of the seeded sequence, pinned as raw states recorded
    /// from the generator tutti-export shipped before the move (xorshift32 with
    /// shifts 13, 17, 5 from 0x12345678, checked outside Rust), so every
    /// export's dither stays bit-identical.
    #[test]
    fn the_seeded_sequence_is_pinned() {
        let mut n = DitherNoise::new();
        let got: Vec<u32> = (0..3).map(|_| n.random()).collect();
        assert_eq!(got, PINNED);
    }

    #[test]
    fn rectangular_stays_within_half_an_lsb() {
        let mut n = DitherNoise::new();
        let mut s = [0.0f32; 4096];
        n.apply(DitherShape::Rectangular, LSB, &mut s);
        assert!(s.iter().all(|x| x.abs() <= 0.5));
        assert!(s.iter().any(|x| x.abs() > 0.4), "the range is used");
    }

    #[test]
    fn triangular_stays_within_one_lsb() {
        let mut n = DitherNoise::new();
        let mut s = [0.0f32; 4096];
        n.apply(DitherShape::Triangular, LSB, &mut s);
        assert!(s.iter().all(|x| x.abs() <= 1.0));
        assert!(s.iter().any(|x| x.abs() > 0.8), "the range is used");
    }

    /// One block of N equals two of N/2.
    #[test]
    fn the_sequence_continues_across_calls() {
        let mut whole = [0.0f32; 16];
        DitherNoise::new().apply(DitherShape::Triangular, LSB, &mut whole);
        let mut split = [0.0f32; 16];
        let mut n = DitherNoise::new();
        let (a, b) = split.split_at_mut(6);
        n.apply(DitherShape::Triangular, LSB, a);
        n.apply(DitherShape::Triangular, LSB, b);
        assert_eq!(whole, split);
    }

    const PINNED: [u32; 3] = [2_274_908_837, 358_294_691, 1_210_119_364];
}
