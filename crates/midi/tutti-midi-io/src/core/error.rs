use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("MIDI parse error: {0}")]
    MidiFileParse(String),

    #[error("Unsupported MIDI timing format")]
    MidiUnsupportedTiming,

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
}

impl From<midly::Error> for Error {
    fn from(e: midly::Error) -> Self {
        Self::MidiFileParse(e.to_string())
    }
}

#[cfg(feature = "midi-hardware")]
impl From<midir::InitError> for Error {
    fn from(e: midir::InitError) -> Self {
        Self::MidiDevice(e.to_string())
    }
}

#[cfg(feature = "midi-hardware")]
impl From<midir::ConnectError<midir::MidiOutput>> for Error {
    fn from(e: midir::ConnectError<midir::MidiOutput>) -> Self {
        Self::MidiPort(e.to_string())
    }
}

#[cfg(feature = "midi-hardware")]
impl From<midir::ConnectError<midir::MidiInput>> for Error {
    fn from(e: midir::ConnectError<midir::MidiInput>) -> Self {
        Self::MidiPort(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
