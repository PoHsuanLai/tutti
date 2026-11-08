//! How a unit is built.

use tutti_types::SampleRate;

/// What a [`SoundFontUnit`](crate::SoundFontUnit) is built with.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct Settings {
    /// The rate to render at, rounded to whole hertz. 16 to 192 kHz.
    pub sample_rate: SampleRate,
    /// The most voices sounding at once: at least 1.
    pub max_polyphony: usize,
    /// Whether the reverb and chorus effect lines run.
    pub reverb_and_chorus: bool,
}

impl Settings {
    /// The polyphony of [`new`](Self::new).
    pub const DEFAULT_MAX_POLYPHONY: usize = 64;

    /// Settings at `sample_rate`: 64 voices, reverb and chorus on.
    #[must_use]
    pub fn new(sample_rate: SampleRate) -> Self {
        Self {
            sample_rate,
            max_polyphony: Self::DEFAULT_MAX_POLYPHONY,
            reverb_and_chorus: true,
        }
    }
}
