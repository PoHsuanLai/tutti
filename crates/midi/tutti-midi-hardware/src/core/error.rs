use thiserror::Error;

/// What can go wrong opening or driving an OS MIDI endpoint.
#[derive(Error, Debug)]
pub enum Error {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("MIDI port error: {0}")]
    MidiPort(String),

    #[error("MIDI device error: {0}")]
    MidiDevice(String),

    #[error("CoreMIDI error: {operation} ({status})")]
    CoreMidi {
        operation: &'static str,
        status: i32,
    },

    #[error("ALSA error: {operation} ({code})")]
    Alsa { operation: &'static str, code: i32 },

    /// No MIDI backend exists for this build.
    ///
    /// Distinct from "no devices found", which is an empty list and not an
    /// error. This says the *platform* has no native-UMP path compiled in —
    /// Windows, or a Linux built against an alsa-lib older than 1.2.10. A caller
    /// that shows the user "no MIDI devices" for this case is hiding a build
    /// fact behind a runtime one.
    #[error("no native-UMP MIDI backend on this platform: {0}")]
    Unsupported(&'static str),

    #[error("Invalid config: {0}")]
    InvalidConfig(String),
    // NOTE: there is deliberately no `File(tutti_midi_file::Error)` variant.
    // It existed only to serve the SMF re-export this crate used to carry, and
    // nothing ever constructed or matched it. A parse failure is not a port
    // failure — a consumer that reads files owns that error itself
    // (`bevy_tutti::midi::file::MidiFileError::Smf` is the live example).
}

// The three `From<midir::*>` impls that used to sit here are gone with midir.
// `MidiPort` and `MidiDevice` survive — the CoreMIDI and ALSA backends raise
// them directly.

pub type Result<T> = std::result::Result<T, Error>;
