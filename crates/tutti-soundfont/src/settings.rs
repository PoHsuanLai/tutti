// Derived from rustysynth 1.3.6 (MIT), Copyright (C) 2022 Nobuaki Tanaka, after
// MeltySynth (Copyright (C) 2021 Nobuaki Tanaka). The MIT notice is
// LICENSE-RUSTYSYNTH.txt, at the root of this crate. In this file: the settings follow rustysynth's synthesizer_settings.rs.

//! How a unit is built.

use std::fmt;

use tutti_midi_protocol::DeviceId;
use tutti_ump::MidiGroup;

/// The most voices a unit sounds at once: 1 to [`MAX`](Self::MAX).
///
/// A pool allocates every voice when it is built, so a count past the ceiling
/// is refused rather than tried; 4096 is far beyond what a SoundFont plays.
/// `new` refuses and never clamps; `of` is for a literal or a constant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Polyphony(usize);

impl Polyphony {
    /// The most voices a unit can be built with.
    pub const MAX: usize = 4096;
    /// The polyphony of [`Settings::new`]: 64 voices.
    pub const DEFAULT: Self = Self(64);

    /// `voices` voices, or `None` for none or more than [`MAX`](Self::MAX).
    #[must_use]
    pub const fn new(voices: usize) -> Option<Self> {
        if voices == 0 || voices > Self::MAX {
            None
        } else {
            Some(Self(voices))
        }
    }

    /// `voices` voices, for a literal or a constant.
    ///
    /// # Panics
    ///
    /// If `voices` is 0 or more than [`MAX`](Self::MAX).
    #[must_use]
    pub const fn of(voices: usize) -> Self {
        match Self::new(voices) {
            Some(polyphony) => polyphony,
            None => panic!("a polyphony is 1 to 4096 voices"),
        }
    }

    /// The number of voices.
    #[must_use]
    pub const fn get(self) -> usize {
        self.0
    }
}

impl Default for Polyphony {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The refused voice count of [`Polyphony`]'s `TryFrom<usize>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolyphonyError(pub usize);

impl fmt::Display for PolyphonyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the polyphony must be between 1 and {} voices, but was {}",
            Polyphony::MAX,
            self.0
        )
    }
}

impl std::error::Error for PolyphonyError {}

impl TryFrom<usize> for Polyphony {
    type Error = PolyphonyError;

    fn try_from(voices: usize) -> Result<Self, PolyphonyError> {
        Self::new(voices).ok_or(PolyphonyError(voices))
    }
}

impl From<Polyphony> for usize {
    fn from(polyphony: Polyphony) -> usize {
        polyphony.0
    }
}

/// What a [`SoundFontUnit`](crate::SoundFontUnit) is built with: start from
/// [`new`](Self::new) and change what differs with the `with_*` methods.
///
/// The rate is not here: a unit renders at the rate its graph prepares it at.
///
/// A unit plays one Group's sixteen channels ([`with_group`](Self::with_group),
/// the first by default): a Group is a channel space of its own (M2-104 section
/// 2.1.2, Appendix B.1), so another Group's messages are not this unit's.
///
/// ```
/// use tutti_soundfont::{Polyphony, Settings};
///
/// let settings = Settings::new().with_polyphony(Polyphony::of(16));
/// assert_eq!(settings.polyphony().get(), 16);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    polyphony: Polyphony,
    group: MidiGroup,
    device: DeviceId,
}

impl Default for Settings {
    fn default() -> Self {
        Self::new()
    }
}

impl Settings {
    /// Settings with 64 voices, playing the first Group.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            polyphony: Polyphony::DEFAULT,
            group: MidiGroup::FIRST,
            device: DeviceId::DEFAULT,
        }
    }

    /// These settings with `polyphony` voices at most sounding at once.
    #[must_use]
    pub const fn with_polyphony(mut self, polyphony: Polyphony) -> Self {
        self.polyphony = polyphony;
        self
    }

    /// These settings playing the channels of `group`, and no other Group's.
    #[must_use]
    pub const fn with_group(mut self, group: MidiGroup) -> Self {
        self.group = group;
        self
    }

    /// These settings answering to the Universal System Exclusive device id
    /// `device` besides the all-call (M1 p.40): a message addressed to it, or to
    /// all, is read (Master Volume, Balance and Tuning, General MIDI System On
    /// and Off). Master Volume and Balance keep what arrives on this id apart
    /// from what arrives on the all-call (M1 p.57).
    #[must_use]
    pub const fn with_device(mut self, device: DeviceId) -> Self {
        self.device = device;
        self
    }

    /// The device id this unit answers to besides the all-call.
    #[must_use]
    pub const fn device(&self) -> DeviceId {
        self.device
    }

    /// The Group this unit plays.
    #[must_use]
    pub const fn group(&self) -> MidiGroup {
        self.group
    }

    /// The most voices sounding at once.
    #[must_use]
    pub const fn polyphony(&self) -> Polyphony {
        self.polyphony
    }
}
