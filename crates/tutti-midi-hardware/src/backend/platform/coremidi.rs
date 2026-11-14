//! The macOS platform: CoreMIDI.

use crate::endpoints::MidiEndpoints;
use crate::error::Error;

pub use super::coremidi::{UmpVirtualDestination, UmpVirtualSource};

/// Returns this platform's MIDI backend.
pub(crate) fn active() -> Box<dyn MidiEndpoints> {
    Box::new(super::coremidi::CoreMidiEndpoints::new())
}

impl From<tutti_coremidi_ffi::Error> for Error {
    fn from(e: tutti_coremidi_ffi::Error) -> Self {
        Self::CoreMidi {
            operation: e.operation,
            status: e.status,
        }
    }
}
