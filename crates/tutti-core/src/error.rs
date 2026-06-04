//! Error types for tutti-core.

use std::string::String;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Invalid config: {0}")]
    InvalidConfig(String),

    #[error("Invalid tempo: {0}. Must be between 20.0 and 999.0 BPM")]
    InvalidTempo(f32),

    #[error("Invalid device: {0}")]
    InvalidDevice(String),

    #[error("LUFS measurement not available (already in use or not initialized)")]
    LufsNotReady,
}

pub type Result<T> = core::result::Result<T, Error>;
