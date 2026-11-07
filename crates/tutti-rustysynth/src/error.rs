use thiserror::Error;
use tutti_types::{SampleRate, Samples};

/// Why a [`Synthesizer`](crate::Synthesizer) could not be built.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SynthesizerError {
    /// The rate is outside 16 to 192 kHz.
    #[error("the sample rate must be between 16000 and 192000 Hz, but was {0}")]
    SampleRateOutOfRange(SampleRate),
    /// The block size is outside 8 to 1024 frames.
    #[error("the block size must be between 8 and 1024 frames, but was {0}")]
    BlockSizeOutOfRange(Samples),
    /// The polyphony is outside 8 to 256 voices.
    #[error("the maximum number of polyphony must be between 8 and 256, but was {0}")]
    MaximumPolyphonyOutOfRange(usize),
}
