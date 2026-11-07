//! How a unit's synthesizer is built.

use tutti_rustysynth::SynthesizerSettings;
use tutti_types::{SampleRate, Samples};

/// The synthesizer settings a [`SoundFontUnit`](crate::SoundFontUnit) is built
/// with. The block size is not one of them: every unit is built at
/// [`SYNTH_BLOCK_FRAMES`](crate::SYNTH_BLOCK_FRAMES).
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct Settings {
    /// The rate to render at, rounded to whole hertz. RustySynth accepts
    /// 16 to 192 kHz.
    pub sample_rate: SampleRate,
    /// The most voices sounding at once.
    pub max_polyphony: usize,
    /// Whether the reverb and chorus effect lines run.
    pub reverb_and_chorus: bool,
}

impl Settings {
    /// Settings at `sample_rate` with RustySynth's defaults: 64 voices, reverb
    /// and chorus on.
    #[must_use]
    pub fn new(sample_rate: SampleRate) -> Self {
        let defaults = SynthesizerSettings::new(sample_rate);
        Self {
            sample_rate,
            max_polyphony: defaults.maximum_polyphony,
            reverb_and_chorus: defaults.enable_reverb_and_chorus,
        }
    }

    /// RustySynth's settings, at the unit's fixed block size.
    pub(crate) fn to_rustysynth(self) -> SynthesizerSettings {
        // `SynthesizerSettings` is `#[non_exhaustive]`, so it is built from
        // `new` and then assigned.
        let mut settings = SynthesizerSettings::new(self.sample_rate);
        settings.maximum_polyphony = self.max_polyphony;
        settings.enable_reverb_and_chorus = self.reverb_and_chorus;
        settings.block_size = Samples(crate::SYNTH_BLOCK_FRAMES);
        settings
    }
}
