//! The crate's error type. The crate root re-exports `SoundFont` and
//! `SoundFontError` (what `SoundFont::new` returns) from `tutti-sf2`.

use thiserror::Error;
use tutti_rustysynth::SynthesizerError;

/// `Result` with this crate's [`enum@Error`] as the failure type.
pub type Result<T> = std::result::Result<T, Error>;

/// What can go wrong loading or configuring a SoundFont unit.
///
/// Once a [`crate::SoundFontUnit`] exists, rendering and forking are
/// infallible.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// Reading the `.sf2` from disk failed.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// The synthesizer refused the [`Settings`](crate::Settings), or the rate
    /// a graph runs at.
    #[error("SoundFont error: {0}")]
    Synthesizer(#[from] SynthesizerError),
}
