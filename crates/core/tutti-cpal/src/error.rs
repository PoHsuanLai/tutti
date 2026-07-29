//! Errors from the device layer.

use thiserror::Error;

/// A failure opening, configuring, or running an audio device.
///
/// Every variant is a device concern. Subsystem failures (synth, plugin,
/// export) are the host's to aggregate — this crate only knows about sound
/// cards, so wrapping those here would make the type a catch-all that every
/// consumer must match exhaustively.
#[derive(Error, Debug)]
pub enum Error {
    /// No default output stream config was available from the host.
    #[error("Audio device not available")]
    DeviceNotAvailable(#[from] cpal::DefaultStreamConfigError),

    /// The CPAL stream could not be built with the requested config.
    #[error("Failed to build audio stream")]
    BuildStream(#[from] cpal::BuildStreamError),

    /// The CPAL stream could not be started.
    #[error("Failed to play audio stream")]
    PlayStream(#[from] cpal::PlayStreamError),

    /// Enumerating audio devices via CPAL failed.
    #[error("Failed to enumerate devices")]
    DevicesError(#[from] cpal::DevicesError),

    /// Querying a device's name via CPAL failed.
    #[error("Failed to get device name")]
    DeviceNameError(#[from] cpal::DeviceNameError),

    /// The requested stream configuration was rejected as invalid.
    #[error("Invalid config: {0}")]
    InvalidConfig(String),

    /// The selected audio device could not be used (missing, unsupported, etc.).
    #[error("Invalid device: {0}")]
    InvalidDevice(String),

    /// I/O failure while reading or writing audio data.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Convenience alias for `Result<T, `[`enum@Error`]`>`.
pub type Result<T> = core::result::Result<T, Error>;
