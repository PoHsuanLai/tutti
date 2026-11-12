//! The crate's [`Error`](enum@Error) and [`Result`].
//!
//! One enum for every OS edge. The two platform arms ([`Error::CoreMidi`],
//! [`Error::Alsa`]) carry the raw OS status alongside the call that produced it,
//! because that pair is what makes a driver failure diagnosable — a bare string
//! loses the code a caller may want to match on.

use thiserror::Error;

/// What can go wrong opening or driving an OS MIDI endpoint.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// A filesystem or descriptor error surfaced from `std::io`.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Opening, configuring or connecting a port failed, with the backend's own
    /// description and, when there was one, the error under it
    /// ([`source`](std::error::Error::source)). Raised directly by the CoreMIDI
    /// and ALSA backends; made with [`Error::port`] and [`Error::port_from`].
    #[error("MIDI port error: {what}")]
    MidiPort {
        /// What failed, in words.
        what: String,
        /// The error under it, if the failure came from one.
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    /// No device is present under this id: what a stale id resolves to.
    #[error("no MIDI device {0}")]
    NoDevice(tutti_midi_device::DeviceId),

    /// The device is present but does not go the way asked: an output opened
    /// as an input, or the reverse.
    #[error("MIDI device {id} is {direction:?}")]
    WrongDirection {
        /// The device.
        id: tutti_midi_device::DeviceId,
        /// Which way it does go.
        direction: tutti_midi_device::PortDirection,
    },

    /// No present device has a name containing this text. Carries the names it
    /// searched (the devices of the direction asked for), so a typo shows what
    /// was there.
    #[error("no MIDI device named like '{name}' (searched: {})", .searched.join(", "))]
    NoDeviceNamed {
        /// The text that matched nothing.
        name: String,
        /// The names of the devices it was matched against; empty when none
        /// was present.
        searched: Vec<String>,
    },

    /// The named device could not be driven, with the backend's own
    /// description and, when there was one, the error under it
    /// ([`source`](std::error::Error::source)). Made with [`Error::device`]
    /// and [`Error::device_from`].
    #[error("MIDI device error: {what}")]
    MidiDevice {
        /// What failed, in words.
        what: String,
        /// The error under it, if the failure came from one.
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

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

    /// Every slot for an input device is taken: a device is an event input of
    /// the graph's ingest node, and it has as many as the session was built
    /// with.
    #[error(transparent)]
    NoFreeSlot(#[from] tutti_midi_runtime::NoFreeSlot),

    /// A backend seeded its device list twice, or after reporting a change.
    #[error(transparent)]
    Seed(#[from] crate::SeedError),

    /// No MIDI backend exists for this build.
    ///
    /// Distinct from "no devices found", which is an empty list and not an
    /// error. This says the *platform* has no native-UMP path compiled in —
    /// Windows, or a Linux built against an alsa-lib older than 1.2.10. A caller
    /// that shows the user "no MIDI devices" for this case is hiding a build
    /// fact behind a runtime one.
    #[error("no native-UMP MIDI backend on this platform: {0}")]
    Unsupported(&'static str),
    // NOTE: there is deliberately no `File(tutti_midi_file::Error)` variant.
    // A parse failure is not a port failure — a consumer that reads files owns
    // that error itself (`tutti_midi_file::Error`, which this crate does not
    // depend on).
}

impl Error {
    /// A [`MidiPort`](Self::MidiPort) error with no error under it.
    #[must_use]
    pub fn port(what: impl Into<String>) -> Self {
        Self::MidiPort {
            what: what.into(),
            source: None,
        }
    }

    /// A [`MidiPort`](Self::MidiPort) error caused by `source`.
    #[must_use]
    pub fn port_from(
        what: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::MidiPort {
            what: what.into(),
            source: Some(Box::new(source)),
        }
    }

    /// A [`MidiDevice`](Self::MidiDevice) error with no error under it.
    #[must_use]
    pub fn device(what: impl Into<String>) -> Self {
        Self::MidiDevice {
            what: what.into(),
            source: None,
        }
    }

    /// A [`MidiDevice`](Self::MidiDevice) error caused by `source`.
    #[must_use]
    pub fn device_from(
        what: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::MidiDevice {
            what: what.into(),
            source: Some(Box::new(source)),
        }
    }
}

/// The crate's result type: [`Error`](enum@Error) on the failure side.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(all(target_os = "linux", alsa_ump))]
impl From<tutti_alsa_ffi::Error> for Error {
    fn from(e: tutti_alsa_ffi::Error) -> Self {
        Self::Alsa {
            operation: e.operation,
            code: e.code,
        }
    }
}

#[cfg(target_os = "macos")]
impl From<tutti_coremidi_ffi::Error> for Error {
    fn from(e: tutti_coremidi_ffi::Error) -> Self {
        Self::CoreMidi {
            operation: e.operation,
            status: e.status,
        }
    }
}

#[cfg(test)]
mod tests;
