// Derived from rustysynth 1.3.6 (MIT), Copyright (C) 2022 Nobuaki Tanaka, after
// MeltySynth (Copyright (C) 2021 Nobuaki Tanaka). The MIT notice is
// LICENSE-RUSTYSYNTH.txt, at the root of this crate. In this file: the errors follow rustysynth's error.rs.

//! The crate's error type.

use thiserror::Error;
use tutti_types::SampleRate;

/// `Result` with this crate's [`enum@Error`] as the failure type.
pub type Result<T> = std::result::Result<T, Error>;

/// What can go wrong loading or configuring a SoundFont unit.
///
/// Once a [`crate::SoundFontUnit`] exists, rendering and forking are
/// infallible.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// Reading the `.sf2` from disk failed. The cause is the
    /// [`source`](std::error::Error::source).
    #[error("reading the SoundFont failed")]
    Io(#[from] std::io::Error),

    /// The rate is outside 16 to 192 kHz.
    #[error("the sample rate must be between 16000 and 192000 Hz, but was {0}")]
    SampleRateOutOfRange(SampleRate),

    /// The unit was asked for no voices.
    #[error("the polyphony must be between 1 and 4096 voices, but was {0}")]
    PolyphonyOutOfRange(usize),
}
