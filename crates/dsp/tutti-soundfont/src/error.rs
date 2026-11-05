//! The crate's error type, kept in its own module so the vendored synthesizer
//! error stops here. Only the file-parse error, `SoundFontError`, is
//! re-exported from the crate root (the type `SoundFont::new` returns).

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

    /// RustySynth refused the `SoundFont` + `SynthesizerSettings` pair.
    ///
    /// Carries a string rather than wrapping `rustysynth::SynthesizerError`
    /// so the vendored type is not part of this crate's error surface.
    #[error("SoundFont error: {0}")]
    SoundFont(String),
}
