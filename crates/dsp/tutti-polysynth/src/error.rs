//! The crate's single error type. Construction can fail, and so can a fork
//! (`PolySynth::fork_instance`); once a [`PolySynth`](crate::PolySynth)
//! exists, rendering it is infallible.

use thiserror::Error;

/// `Result` with this crate's [`Error`](enum@Error) as the error type.
pub type Result<T> = std::result::Result<T, Error>;

/// What can go wrong building a [`PolySynth`](crate::PolySynth), or forking
/// one.
#[derive(Debug, Error)]
pub enum Error {
    /// A [`SynthConfig`](crate::SynthConfig) field is out of range — today only
    /// `max_voices`, which must be between 1 and the inline voice ceiling.
    /// The message names the offending field.
    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),

    /// A fork for an offline render could not carry the MIDI source the
    /// synth plays: one is installed on its port that cannot be rebound onto
    /// the render's timeline (`MidiUnitIn::rebind_offline` answered `None`),
    /// so the render would drop its notes. Refused rather than rendered as
    /// silence.
    #[error("the synth plays a MIDI source that cannot be rebound onto an offline render")]
    MidiSource,
}
