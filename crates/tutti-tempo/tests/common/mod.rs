#![allow(dead_code)]
use tutti_tempo::{Shape, TempoPoint};
use tutti_types::{Beat, Bpm, SampleRate};

pub const R: SampleRate = SampleRate(48_000.0);

pub fn pt(beat: f64, tempo: f64, shape: Shape) -> TempoPoint {
    TempoPoint {
        beat: Beat(beat),
        tempo: Bpm::of(tempo),
        shape,
    }
}

/// A fixed-seed generator: the same sequence on every platform.
pub struct Lcg(pub u64);

impl Lcg {
    /// Uniform in `[0, 1)`.
    pub fn next(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}
