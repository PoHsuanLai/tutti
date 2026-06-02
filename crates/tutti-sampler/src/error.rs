//! Error types.

use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Sample not found: {0}")]
    SampleNotFound(String),

    #[error("Recording error: {0}")]
    Recording(String),

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

    #[error("Memory-mapped reader error: {0}")]
    MmapReader(String),
}

pub type Result<T> = std::result::Result<T, Error>;
