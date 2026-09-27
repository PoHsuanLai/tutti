//! The crate's error type. Only construction can fail; once a
//! [`PolySynth`](crate::PolySynth) exists, rendering and forking it are
//! infallible.

use thiserror::Error;

/// `Result` with this crate's [`Error`](enum@Error) as the error type.
pub type Result<T> = std::result::Result<T, Error>;

/// The error returned when a [`PolySynth`](crate::PolySynth) cannot be built.
#[derive(Debug, Error)]
pub enum Error {
    /// A [`SynthConfig`](crate::SynthConfig) field is out of range: currently
    /// only a `max_voices` of 0. The message names the offending field.
    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),
}
