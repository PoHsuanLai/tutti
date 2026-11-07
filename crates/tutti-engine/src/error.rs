//! Error types for tutti-engine.

use std::string::String;
use thiserror::Error;

/// What tutti-engine's fallible operations report.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// An engine configuration value was rejected; the string names which.
    #[error("Invalid config: {0}")]
    InvalidConfig(String),

    /// The named audio device could not be used.
    #[error("Invalid device: {0}")]
    InvalidDevice(String),

    /// The LUFS meter is held by another reader, or was never initialized.
    #[error("LUFS measurement not available (already in use or not initialized)")]
    LufsNotReady,

    /// [`AudioTap::open`](tutti_metering::AudioTap::open) was refused — the tap already
    /// has a consumer.
    ///
    /// Transparent, so [`TapBusy`](tutti_metering::TapBusy) stays the precise type for a
    /// caller that wants to match on it while `?` still composes into this
    /// crate's `Result`.
    #[error(transparent)]
    TapBusy(#[from] tutti_metering::TapBusy),
}

/// `Result` with this crate's [`Error`](enum@Error) as the error type.
pub type Result<T> = core::result::Result<T, Error>;
