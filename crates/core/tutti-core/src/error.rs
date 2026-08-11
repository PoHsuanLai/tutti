//! Error types for tutti-core.

use std::string::String;
use thiserror::Error;

/// What tutti-core's fallible operations report.
#[derive(Error, Debug)]
pub enum Error {
    /// An engine configuration value was rejected; the string names which.
    #[error("Invalid config: {0}")]
    InvalidConfig(String),

    /// A tempo outside the accepted 20.0..=999.0 BPM range, in [`Bpm`]'s units.
    ///
    /// [`Bpm`]: crate::Bpm
    #[error("Invalid tempo: {0}. Must be between 20.0 and 999.0 BPM")]
    InvalidTempo(f32),

    /// The named audio device could not be used.
    #[error("Invalid device: {0}")]
    InvalidDevice(String),

    /// The LUFS meter is held by another reader, or was never initialized.
    #[error("LUFS measurement not available (already in use or not initialized)")]
    LufsNotReady,

    /// [`AudioTap::open`](crate::AudioTap::open) was refused — the tap already
    /// has a consumer.
    ///
    /// Transparent, so [`TapBusy`](crate::TapBusy) stays the precise type for a
    /// caller that wants to match on it while `?` still composes into this
    /// crate's `Result`. Without this variant, opening a tap inside a function
    /// that already returns `tutti_core::Result` needed a manual `map_err`.
    #[error(transparent)]
    TapBusy(#[from] crate::TapBusy),
}

/// `Result` with this crate's [`Error`](enum@Error) as the error type.
pub type Result<T> = core::result::Result<T, Error>;
