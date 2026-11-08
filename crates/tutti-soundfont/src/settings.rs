// Derived from rustysynth 1.3.6 (MIT), Copyright (C) 2022 Nobuaki Tanaka, after
// MeltySynth (Copyright (C) 2021 Nobuaki Tanaka). The MIT notice is
// LICENSE-RUSTYSYNTH.txt, at the root of this crate. In this file: the settings follow rustysynth's synthesizer_settings.rs.

//! How a unit is built.

use tutti_types::SampleRate;

/// What a [`SoundFontUnit`](crate::SoundFontUnit) is built with.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct Settings {
    /// The rate to render at, rounded to whole hertz. 16 to 192 kHz.
    pub sample_rate: SampleRate,
    /// The most voices sounding at once: 1 to [`MAX_POLYPHONY`](Self::MAX_POLYPHONY).
    pub max_polyphony: usize,
    /// Whether the reverb and chorus effect lines run.
    pub reverb_and_chorus: bool,
}

impl Settings {
    /// The most voices a unit can be built with. A pool allocates every voice
    /// when it is built, so a count past this is refused rather than tried;
    /// 4096 is far beyond what a SoundFont plays.
    pub const MAX_POLYPHONY: usize = 4096;
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
