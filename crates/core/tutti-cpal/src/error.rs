//! Errors from the device layer.

use thiserror::Error;

/// A failure opening, configuring, or running an audio device.
///
/// Every variant is a device concern; failures from other subsystems (synth,
/// plugin, export) are the host's to aggregate. Errors reported by a stream
/// that is already running do not come back as an `Error`: CPAL delivers them
/// to [`StreamFaults`](crate::StreamFaults).
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
    /// A runtime error rather than a compile error: [`AudioHost`](crate::AudioHost)
    /// has every variant on every platform, and
    /// [`available_hosts`](crate::available_hosts) lists the reachable ones.
    #[error("audio host {host} unavailable: {reason}")]
    HostUnavailable {
        /// The host that was asked for.
        host: crate::host::AudioHost,
        /// Why it cannot be opened, as the backend or this crate put it.
        reason: String,
    },

    /// A capture device cannot run at the graph's sample rate.
    ///
    /// Returned by `MicIn::open` (feature `capture`): the mic is rendered into
    /// the graph with no resampling, so it must run at the graph's rate.
    #[error("input device {device_name:?} runs at {device} Hz; the graph runs at {graph} Hz")]
    SampleRateMismatch {
        /// The input device's name, as the OS reports it.
        device_name: String,
        /// The rate of the device's default input config.
        device: tutti_core::SampleRate,
        /// The rate that was asked for.
        graph: tutti_core::SampleRate,
    },

    /// A plain [`TuttiDriver::restart`](crate::TuttiDriver::restart) found
    /// the output device at a different rate than the graph was built at.
    ///
    /// Everything time-denominated in the graph — every oscillator, every
    /// delay in samples, the beat clock — would run at the graph's rate on
    /// the new device, off pitch and off tempo with no error anywhere, so the
    /// stream is left stopped instead. Restart with
    /// [`TuttiDriver::restart_with`](crate::TuttiDriver::restart_with) and
    /// re-rate the graph in its hook.
    #[error(
        "the output device now runs at {device} Hz, the graph at {graph} Hz; \
         restart with `restart_with` and re-rate the graph"
    )]
    RateChanged {
        /// The rate the output device now reports.
        device: tutti_core::SampleRate,
        /// The rate the graph runs at ([`TuttiDriver::graph_rate`](crate::TuttiDriver::graph_rate)).
        graph: tutti_core::SampleRate,
    },

    /// I/O failure while reading or writing audio data.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Convenience alias for `Result<T, `[`enum@Error`]`>`.
pub type Result<T> = core::result::Result<T, Error>;
