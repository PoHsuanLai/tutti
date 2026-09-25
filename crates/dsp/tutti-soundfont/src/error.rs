//! The crate's error type, kept in its own module so the vendored RustySynth
//! error surface stops here rather than being re-exported.

use thiserror::Error;

/// `Result` with this crate's [`enum@Error`] as the failure type.
pub type Result<T> = std::result::Result<T, Error>;

/// What can go wrong loading or configuring a SoundFont unit, or forking one.
///
/// Once a [`crate::SoundFontUnit`] exists, rendering is infallible; only a
/// fork of it (`SoundFontUnit::fork_instance`) can still fail.
#[derive(Debug, Error)]
pub enum Error {
    /// A fork for an offline render could not carry the MIDI source the unit
    /// plays: one is installed on its port that cannot be rebound onto the
    /// render's timeline (`MidiUnitIn::rebind_offline` answered `None`), so
    /// the render would drop its notes. Refused rather than rendered as
    /// silence.
    #[error(
        "the SoundFont unit plays a MIDI source that cannot be rebound onto an offline render"
    )]
    MidiSource,

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
