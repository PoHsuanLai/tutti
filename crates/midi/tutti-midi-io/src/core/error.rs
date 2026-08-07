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

    /// A MIDI **file** error, from [`tutti_midi_file`].
    ///
    /// Kept reachable because this crate re-exports the file codecs, so callers
    /// written against `tutti_midi_io::smf` also expect `tutti_midi_io::Result`.
    /// It is a wrapped variant rather than flattened copies of the file crate's
    /// cases: a parse failure is not a port failure, and collapsing them would
    /// make an unreadable `.mid` and an unopenable device indistinguishable in a
    /// match.
    #[error("MIDI file: {0}")]
    File(#[from] tutti_midi_file::Error),
}

// The three `From<midir::*>` impls that used to sit here are gone with midir.
// `MidiPort` and `MidiDevice` survive — the CoreMIDI and ALSA backends raise
// them directly.

pub type Result<T> = std::result::Result<T, Error>;
