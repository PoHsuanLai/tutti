use std::io;
use thiserror::Error;

#[derive(Error, Debug)]
#[non_exhaustive]
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

    #[error("Unsupported channel count: {0} (this export path does not support that width)")]
    UnsupportedChannels(u16),

    /// A loudness measurement was asked for and could not be taken — EBU R128
    /// accepts 1–64 channels at 16 Hz–2.8 MHz.
    ///
    /// An error rather than a skipped measurement, because a normalized export
    /// that quietly writes un-normalized audio reports success and leaves no
    /// trace: nothing in [`Written`](crate::Written) records that the gain the
    /// caller asked for was never applied.
    #[error("Cannot measure loudness: {0}")]
    Unmeasurable(String),
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
