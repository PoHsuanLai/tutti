//! Error types for tutti-polysynth.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),
}
