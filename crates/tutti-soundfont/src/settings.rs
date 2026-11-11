// Derived from rustysynth 1.3.6 (MIT), Copyright (C) 2022 Nobuaki Tanaka, after
// MeltySynth (Copyright (C) 2021 Nobuaki Tanaka). The MIT notice is
// LICENSE-RUSTYSYNTH.txt, at the root of this crate. In this file: the settings follow rustysynth's synthesizer_settings.rs.

//! How a unit is built.

use std::num::NonZeroUsize;

use tutti_types::SampleRate;
use tutti_ump::MidiGroup;

/// What a [`SoundFontUnit`](crate::SoundFontUnit) is built with: start from
/// [`new`](Self::new) and change what differs with the `with_*` methods.
///
/// A unit plays one Group's sixteen channels ([`with_group`](Self::with_group),
/// the first by default): a Group is a channel space of its own (M2-104 section
/// 2.1.2, Appendix B.1), so another Group's messages are not this unit's.
///
/// ```
/// use std::num::NonZeroUsize;
/// use tutti_soundfont::Settings;
/// use tutti_types::SampleRate;
///
/// let settings = Settings::new(SampleRate::SR_48K)
///     .with_polyphony(NonZeroUsize::new(16).unwrap());
/// assert_eq!(settings.polyphony().get(), 16);
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    sample_rate: SampleRate,
    polyphony: NonZeroUsize,
    group: MidiGroup,
}

impl Settings {
    /// The most voices a unit can be built with. A pool allocates every voice
    /// when it is built, so a count past this is refused rather than tried;
    /// 4096 is far beyond what a SoundFont plays.
    pub const MAX_POLYPHONY: usize = 4096;
    /// The polyphony of [`new`](Self::new).
    pub const DEFAULT_POLYPHONY: NonZeroUsize = NonZeroUsize::new(64).unwrap();

    /// Settings at `sample_rate`: 64 voices.
    #[must_use]
    pub const fn new(sample_rate: SampleRate) -> Self {
        Self {
            sample_rate,
            polyphony: Self::DEFAULT_POLYPHONY,
            group: MidiGroup::FIRST,
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

    /// These settings playing the channels of `group`, and no other Group's.
    #[must_use]
    pub const fn with_group(mut self, group: MidiGroup) -> Self {
        self.group = group;
        self
    }

    /// The Group this unit plays.
    #[must_use]
    pub const fn group(&self) -> MidiGroup {
        self.group
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
}
