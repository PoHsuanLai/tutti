//! Sample-rate conversion on the way out: [`Resample`].

use crate::process::ChunkSize;
use tutti_types::SampleRate;

/// Sample-rate conversion applied on the way out.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Resample {
    /// Rate the file is written at. Equal to the render rate means no
    /// conversion — the comparison is made on the integer rate the codec headers
    /// speak, so two rates an ULP apart are the same rate.
    pub target_rate: SampleRate,
    /// Input FRAMES per FFT chunk, and how finely each is subdivided. Sets the
    /// converter's anti-alias steepness and its latency.
    pub chunk: ChunkSize,
}

impl Resample {
    /// Convert to `target_rate` with the default [`ChunkSize`].
    pub fn to(target_rate: impl Into<SampleRate>) -> Self {
        Self {
            target_rate: target_rate.into(),
            chunk: ChunkSize::default(),
        }
    }
}
