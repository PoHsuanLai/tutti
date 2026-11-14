//! The Linux platform: ALSA's UMP sequencer.

use crate::endpoints::MidiEndpoints;
use crate::error::Error;

/// Returns this platform's MIDI backend.
pub(crate) fn active() -> Box<dyn MidiEndpoints> {
    Box::new(super::alsa::AlsaEndpoints::new())
}

impl From<tutti_alsa_ffi::Error> for Error {
    fn from(e: tutti_alsa_ffi::Error) -> Self {
        Self::Alsa {
            operation: e.operation,
            code: e.code,
        }
    }
}
