use std::io;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("Unsupported format: {0}")]
    UnsupportedFormat(String),

    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("Encoding error: {0}")]
    Encoding(String),

    #[error("Render error: {0}")]
    Render(String),

    #[error("Resampling error: {0}")]
    Resample(String),

    #[error("Invalid audio data: {0}")]
    InvalidData(String),

    #[error("Cancelled")]
    Cancelled,
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(feature = "wav")]
impl From<hound::Error> for Error {
    fn from(e: hound::Error) -> Self {
        Self::Io(io::Error::other(e))
    }
}

impl From<rubato::ResamplerConstructionError> for Error {
    fn from(e: rubato::ResamplerConstructionError) -> Self {
        Self::Resample(e.to_string())
    }
}

impl From<rubato::ResampleError> for Error {
    fn from(e: rubato::ResampleError) -> Self {
        Self::Resample(e.to_string())
    }
}
