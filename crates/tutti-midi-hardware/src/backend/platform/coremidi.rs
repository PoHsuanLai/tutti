//! The macOS platform: CoreMIDI.

use crate::endpoints::MidiEndpoints;
use crate::error::Error;

pub use super::coremidi::{UmpVirtualDestination, UmpVirtualSource};

/// Returns this platform's MIDI backend.
///
/// Returns a boxed trait object rather than an `impl Trait` so the return type
/// does not change per platform: a caller stores it in one field.
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
