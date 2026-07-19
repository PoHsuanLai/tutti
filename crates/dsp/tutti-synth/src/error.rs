//! Error types for tutti-synth.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[cfg(feature = "soundfont")]
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),

    #[cfg(feature = "soundfont")]
    #[error("SoundFont error: {0}")]
    SoundFont(String),
}
