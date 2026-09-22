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

    /// The requested audio host is not reachable in this build or on this
    /// machine — the feature is off, the platform has no such host, or the
    /// server is not running.
    ///
    /// A runtime error rather than a compile error on purpose: `AudioHost`
    /// carries every variant on every platform so a host's configuration
    /// struct does not change shape per OS. See [`AudioHost`](crate::AudioHost).
    #[error("audio host {host} unavailable: {reason}")]
    HostUnavailable {
        host: crate::host::AudioHost,
        reason: String,
    },

    /// A capture device cannot run at the graph's sample rate.
    ///
    /// `MicMonitorNode` renders the mic into the graph with no resampling —
    /// its `set_sample_rate` is a documented no-op that assumes the device
    /// layer opened the mic at the graph's rate. This is the error that makes
    /// that an enforced guarantee rather than an assumption.
    #[error("input device {device_name:?} runs at {device} Hz; the graph runs at {graph} Hz")]
    SampleRateMismatch {
        device_name: String,
        device: tutti_core::SampleRate,
        graph: tutti_core::SampleRate,
    },

    /// I/O failure while reading or writing audio data.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Convenience alias for `Result<T, `[`enum@Error`]`>`.
pub type Result<T> = core::result::Result<T, Error>;
