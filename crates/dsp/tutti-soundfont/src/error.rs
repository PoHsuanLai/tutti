//! The crate's error type, kept in its own module so the vendored RustySynth
//! error surface stops here rather than being re-exported.

use thiserror::Error;

/// `Result` with this crate's [`enum@Error`] as the failure type.
pub type Result<T> = std::result::Result<T, Error>;

/// What can go wrong loading or configuring a SoundFont unit.
///
/// Construction-time only — once a [`crate::SoundFontUnit`] exists, rendering is
/// infallible.
#[derive(Debug, Error)]
pub enum Error {
    /// Reading the `.sf2` from disk failed.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// RustySynth refused the `SoundFont` + `SynthesizerSettings` pair.
    ///
    /// Carries a string rather than wrapping `rustysynth::SynthesizerError`
    /// so this crate's error surface does not re-export the vendored type.
    #[error("SoundFont error: {0}")]
    SoundFont(String),
}
