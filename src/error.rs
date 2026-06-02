//! Centralized error type for the tutti umbrella crate.
//!
//! Wraps all subsystem errors so `?` propagates naturally across crate boundaries.

use thiserror::Error;

/// Aggregate error type covering every subsystem re-exported by `tutti`.
///
/// Each variant either wraps a subsystem error via `#[from]` (so `?` works
/// across crate boundaries) or captures an audio I/O failure surfaced by
/// [`cpal`].
#[derive(Error, Debug)]
pub enum Error {
    /// Wraps an error from [`tutti_core::Error`].
    #[error(transparent)]
    Core(#[from] tutti_core::Error),

    /// No default output stream config was available from the host.
    ///
    /// Wraps [`cpal::DefaultStreamConfigError`].
    #[cfg(feature = "std")]
    #[error("Audio device not available")]
    DeviceNotAvailable(#[from] cpal::DefaultStreamConfigError),

    /// The CPAL stream could not be built with the requested config.
    ///
    /// Wraps [`cpal::BuildStreamError`].
    #[cfg(feature = "std")]
    #[error("Failed to build audio stream")]
    BuildStream(#[from] cpal::BuildStreamError),

    /// The CPAL stream could not be started.
    ///
    /// Wraps [`cpal::PlayStreamError`].
    #[cfg(feature = "std")]
    #[error("Failed to play audio stream")]
    PlayStream(#[from] cpal::PlayStreamError),

    /// Enumerating audio devices via CPAL failed.
    ///
    /// Wraps [`cpal::DevicesError`].
    #[cfg(feature = "std")]
    #[error("Failed to enumerate devices")]
    DevicesError(#[from] cpal::DevicesError),

    /// Querying a device's name via CPAL failed.
    ///
    /// Wraps [`cpal::DeviceNameError`].
    #[cfg(feature = "std")]
    #[error("Failed to get device name")]
    DeviceNameError(#[from] cpal::DeviceNameError),

    /// The requested stream configuration was rejected as invalid.
    #[cfg(feature = "std")]
    #[error("Invalid config: {0}")]
    InvalidConfig(String),

    /// The selected audio device could not be used (missing, unsupported, etc.).
    #[cfg(feature = "std")]
    #[error("Invalid device: {0}")]
    InvalidDevice(String),

    /// MIDI subsystem failure.
    ///
    /// Wraps [`tutti_midi_io::Error`].
    #[cfg(feature = "midi")]
    #[error("MIDI: {0}")]
    Midi(#[from] tutti_midi_io::Error),

    /// Software-synth subsystem failure.
    ///
    /// Wraps [`tutti_synth::Error`].
    #[cfg(feature = "synth")]
    #[error("Synth: {0}")]
    Synth(#[from] tutti_synth::Error),

    /// Sampler subsystem failure.
    ///
    /// Wraps [`tutti_sampler::Error`].
    #[cfg(feature = "sampler")]
    #[error("Sampler: {0}")]
    Sampler(#[from] tutti_sampler::Error),

    /// Built-in units subsystem failure.
    ///
    /// Wraps [`tutti_units::Error`].
    #[error("Units: {0}")]
    Dsp(#[from] tutti_units::Error),

    /// Plugin-host bridge failure (VST2/VST3/CLAP).
    ///
    /// Wraps [`tutti_plugin::BridgeError`].
    #[cfg(feature = "plugin")]
    #[error("Plugin: {0}")]
    Plugin(#[from] tutti_plugin::BridgeError),

    /// Offline rendering / export subsystem failure.
    ///
    /// Wraps [`tutti_export::Error`].
    #[cfg(feature = "export")]
    #[error("Export: {0}")]
    Export(#[from] tutti_export::Error),

    /// Standard I/O error encountered while reading or writing audio data.
    ///
    /// Wraps [`std::io::Error`].
    #[cfg(feature = "std")]
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Convenience alias for `Result<T, `[`enum@Error`]`>` used throughout this crate.
pub type Result<T> = core::result::Result<T, Error>;
