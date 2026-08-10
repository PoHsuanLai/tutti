//! The crate's [`Error`](enum@Error) and [`Result`].
//!
//! One enum for every OS edge. The two platform arms ([`Error::CoreMidi`],
//! [`Error::Alsa`]) carry the raw OS status alongside the call that produced it,
//! because that pair is what makes a driver failure diagnosable — a bare string
//! loses the code a caller may want to match on.

use thiserror::Error;

/// What can go wrong opening or driving an OS MIDI endpoint.
#[derive(Error, Debug)]
pub enum Error {
    /// A filesystem or descriptor error surfaced from `std::io`.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Opening, configuring or connecting a port failed, with the backend's own
    /// description. Raised directly by the CoreMIDI and ALSA backends.
    #[error("MIDI port error: {0}")]
    MidiPort(String),

    /// The named device could not be found or could not be driven — including
    /// "no endpoint has this [`EndpointId`](crate::EndpointId)", which is what
    /// a stale id resolves to.
    #[error("MIDI device error: {0}")]
    MidiDevice(String),

    /// A CoreMIDI call returned a non-zero `OSStatus`.
    #[error("CoreMIDI error: {operation} ({status})")]
    CoreMidi {
        /// The CoreMIDI call that failed, as a static label
        /// (`"create input port"`, `"send event list"`).
        operation: &'static str,
        /// The `OSStatus` CoreMIDI returned. `-1` is the crate's own stand-in
        /// for "the client was never created".
        status: i32,
    },

    /// An alsa-lib call returned a negative error code.
    #[error("ALSA error: {operation} ({code})")]
    Alsa {
        /// The alsa-lib function that failed, as a static label
        /// (`"snd_seq_open"`, `"snd_seq_connect_to"`).
        operation: &'static str,
        /// alsa-lib's negative return, which is a negated `errno`. Rendered for
        /// humans by `snd_strerror`.
        code: i32,
    },

    /// No MIDI backend exists for this build.
    ///
    /// Distinct from "no devices found", which is an empty list and not an
    /// error. This says the *platform* has no native-UMP path compiled in —
    /// Windows, or a Linux built against an alsa-lib older than 1.2.10. A caller
    /// that shows the user "no MIDI devices" for this case is hiding a build
    /// fact behind a runtime one.
    #[error("no native-UMP MIDI backend on this platform: {0}")]
    Unsupported(&'static str),

    /// A caller-supplied setting the backend cannot honour.
    #[error("Invalid config: {0}")]
    InvalidConfig(String),
    // NOTE: there is deliberately no `File(tutti_midi_file::Error)` variant.
    // A parse failure is not a port failure — a consumer that reads files owns
    // that error itself (`bevy_tutti::midi::file::MidiFileError::Smf` is the
    // live example).
}

/// The crate's result type: [`Error`](enum@Error) on the failure side.
pub type Result<T> = std::result::Result<T, Error>;
