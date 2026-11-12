// Derived from rustysynth 1.3.6 (MIT), Copyright (C) 2022 Nobuaki Tanaka, after
// MeltySynth (Copyright (C) 2021 Nobuaki Tanaka). The MIT notice is
// LICENSE-RUSTYSYNTH.txt, at the root of this crate. In this file: the errors follow rustysynth's error.rs.

//! The crate's error type.

use thiserror::Error;

/// `Result` with this crate's [`enum@Error`] as the failure type.
pub type Result<T> = std::result::Result<T, Error>;

/// What can go wrong building a SoundFont unit.
///
/// Once a [`crate::SoundFontUnit`] exists, rendering and forking are
/// infallible.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// The font has no preset that can be played: every one has a bank or a
    /// program that is not MIDI's (SF2 2.04 section 7.2).
    #[error("the SoundFont has no playable preset")]
    NoPlayablePreset,

    /// [`SoundFontUnit::select`](crate::SoundFontUnit::select) was given a
    /// preset whose bank or program is not MIDI's (section 7.2): it cannot be
    /// played.
    #[error("the preset has no MIDI bank and program, so it cannot be selected")]
    UnplayablePreset,
}
