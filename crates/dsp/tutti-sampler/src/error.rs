//! Error types.

use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Sample not found: {0}")]
    SampleNotFound(String),

    #[error(transparent)]
    Recording(#[from] RecordingError),

    #[error("Failed to enumerate audio devices")]
    DevicesError(#[from] cpal::DevicesError),

    #[error("Failed to get audio device config")]
    DeviceConfigError(#[from] cpal::DefaultStreamConfigError),

    #[error("Failed to build audio stream")]
    BuildStreamError(#[from] cpal::BuildStreamError),

    #[error("Failed to play audio stream")]
    PlayStreamError(#[from] cpal::PlayStreamError),

    #[error("Audio device not found: {0}")]
    DeviceNotFound(String),

    #[error("Hound error: {0}")]
    HoundError(#[from] hound::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Failure modes of the audio-input capture recorder.
#[derive(Error, Debug)]
pub enum RecordingError {
    /// No recording session exists on the requested channel.
    #[error("No recording session on channel {channel}")]
    NoActiveSession { channel: usize },

    /// A session exists on the channel but has been deactivated.
    #[error("Recording session on channel {channel} is not active")]
    SessionInactive { channel: usize },

    /// The session on the channel is recording a different source than the
    /// operation expected.
    #[error("Channel {channel} is not recording {expected:?} (source: {actual:?})")]
    SourceMismatch {
        channel: usize,
        expected: crate::capture::Source,
        actual: crate::capture::Source,
    },

    /// The recordings directory could not be created.
    #[error("failed to create recordings dir {path}: {source}")]
    CreateDirFailed {
        path: std::path::PathBuf,
        source: std::io::Error,
    },

    /// A command could not be delivered to the butler thread.
    #[error("failed to send {command} to butler: {reason}")]
    ChannelSendFailed {
        command: &'static str,
        reason: String,
    },

    /// The audio-input session has no capture ID recorded.
    #[error("No capture ID for audio input recording")]
    NoCaptureId,

    /// The audio-input session has no recording file path recorded.
    #[error("No recording file path")]
    NoRecordingFile,
}
