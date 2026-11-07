use tutti_types::{SampleRate, Samples};

use crate::error::SynthesizerError;

/// What a [`Synthesizer`](crate::Synthesizer) is built with.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct SynthesizerSettings {
    /// The rate to render at, rounded to whole hertz. 16 to 192 kHz.
    pub sample_rate: SampleRate,
    /// The frames rendered at a time: events take effect on this grid. 8 to 1024.
    pub block_size: Samples,
    /// The most voices sounding at once. 8 to 256.
    pub maximum_polyphony: usize,
    /// Whether the reverb and chorus run.
    pub enable_reverb_and_chorus: bool,
}

impl SynthesizerSettings {
    /// The block size of [`new`](Self::new).
    pub const DEFAULT_BLOCK_SIZE: Samples = Samples(64);
    /// The polyphony of [`new`](Self::new).
    pub const DEFAULT_MAXIMUM_POLYPHONY: usize = 64;

    /// Settings at `sample_rate`: a 64-frame block, 64 voices, reverb and
    /// chorus on.
    #[must_use]
    pub fn new(sample_rate: SampleRate) -> Self {
        Self {
            sample_rate,
            block_size: Self::DEFAULT_BLOCK_SIZE,
            maximum_polyphony: Self::DEFAULT_MAXIMUM_POLYPHONY,
            enable_reverb_and_chorus: true,
        }
    }

    /// The rate in whole hertz. The cast saturates, and `validate` refuses a
    /// rate outside the range.
    pub(crate) fn hz(&self) -> i32 {
        self.sample_rate.get().round() as i32
    }

    pub(crate) fn block_len(&self) -> usize {
        self.block_size.get()
    }

    pub(crate) fn validate(&self) -> Result<(), SynthesizerError> {
        if !(16_000..=192_000).contains(&self.hz()) {
            return Err(SynthesizerError::SampleRateOutOfRange(self.sample_rate));
        }
        if !(8..=1024).contains(&self.block_len()) {
            return Err(SynthesizerError::BlockSizeOutOfRange(self.block_size));
        }
        if !(8..=256).contains(&self.maximum_polyphony) {
            return Err(SynthesizerError::MaximumPolyphonyOutOfRange(
                self.maximum_polyphony,
            ));
        }
        Ok(())
    }
}
