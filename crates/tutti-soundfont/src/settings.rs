//! How a unit is built.

use std::num::NonZeroUsize;

use tutti_types::SampleRate;

/// What a [`SoundFontUnit`](crate::SoundFontUnit) is built with: start from
/// [`new`](Self::new) and change what differs with the `with_*` methods.
///
/// ```
/// use std::num::NonZeroUsize;
/// use tutti_soundfont::Settings;
/// use tutti_types::SampleRate;
///
/// let settings = Settings::new(SampleRate::SR_48K)
///     .with_polyphony(NonZeroUsize::new(16).unwrap())
///     .with_reverb_and_chorus(false);
/// assert_eq!(settings.polyphony().get(), 16);
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    sample_rate: SampleRate,
    polyphony: NonZeroUsize,
    reverb_and_chorus: bool,
}

impl Settings {
    /// The polyphony of [`new`](Self::new).
    pub const DEFAULT_POLYPHONY: NonZeroUsize = NonZeroUsize::new(64).unwrap();

    /// Settings at `sample_rate`: 64 voices, reverb and chorus on.
    #[must_use]
    pub const fn new(sample_rate: SampleRate) -> Self {
        Self {
            sample_rate,
            polyphony: Self::DEFAULT_POLYPHONY,
            reverb_and_chorus: true,
        }
    }

    /// These settings at `sample_rate`, rounded to whole hertz when the unit is
    /// built. A rate outside 16 to 192 kHz is refused then, not here.
    #[must_use]
    pub const fn with_sample_rate(mut self, sample_rate: SampleRate) -> Self {
        self.sample_rate = sample_rate;
        self
    }

    /// These settings with `polyphony` voices at most sounding at once.
    #[must_use]
    pub const fn with_polyphony(mut self, polyphony: NonZeroUsize) -> Self {
        self.polyphony = polyphony;
        self
    }

    /// These settings with the reverb and chorus effect lines on or off.
    #[must_use]
    pub const fn with_reverb_and_chorus(mut self, on: bool) -> Self {
        self.reverb_and_chorus = on;
        self
    }

    /// The rate to render at.
    #[must_use]
    pub const fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    /// The most voices sounding at once.
    #[must_use]
    pub const fn polyphony(&self) -> NonZeroUsize {
        self.polyphony
    }

    /// Whether the reverb and chorus effect lines run.
    #[must_use]
    pub const fn reverb_and_chorus(&self) -> bool {
        self.reverb_and_chorus
    }
}
