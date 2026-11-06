//! The crate's error type. RustySynth's `SynthesizerError` stops here, as a
//! string. The crate root re-exports `SoundFont` and `SoundFontError` (what
//! `SoundFont::new` returns) from RustySynth.

use thiserror::Error;

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

    /// RustySynth refused the `SoundFont` and [`Settings`](crate::Settings) pair.
    ///
    /// Carries a string so the forked `SynthesizerError` is not part of this
    /// crate's error surface.
    #[error("SoundFont error: {0}")]
    SoundFont(String),
}
