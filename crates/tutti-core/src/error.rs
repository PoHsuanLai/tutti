//! Error types for tutti-core.

use crate::compat::String;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Invalid config: {0}")]
    InvalidConfig(String),

    #[error("Invalid tempo: {0}. Must be between 20.0 and 999.0 BPM")]
    InvalidTempo(f32),

    #[error("Invalid beat position: {0}. Must be non-negative")]
    InvalidBeat(f64),

    #[error("Invalid loop range: start={start}, end={end}")]
    InvalidLoopRange { start: f64, end: f64 },

    #[error("Invalid time signature: {numerator}/{denominator}")]
    InvalidTimeSignature { numerator: u32, denominator: u32 },

    #[error("Invalid device: {0}")]
    InvalidDevice(String),

    #[error("Lock poisoned")]
    LockPoisoned,

    #[error("Not implemented: {0}")]
    NotImplemented(String),

    #[error("LUFS measurement not available (already in use or not initialized)")]
    LufsNotReady,

    #[error("Synth error: {0}")]
    Synth(String),
}

pub type Result<T> = core::result::Result<T, Error>;
