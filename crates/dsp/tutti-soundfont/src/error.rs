//! Error types for tutti-soundfont.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// RustySynth refused the `SoundFont` + `SynthesizerSettings` pair.
    ///
    /// Carries a string rather than wrapping `rustysynth::SynthesizerError`
    /// so this crate's error surface does not re-export the vendored type.
    #[error("SoundFont error: {0}")]
    SoundFont(String),
}
