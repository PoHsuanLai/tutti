//! The Linux platform: ALSA's UMP sequencer.

use crate::endpoints::MidiEndpoints;
use crate::error::Error;

/// Returns this platform's MIDI backend.
///
/// Returns a boxed trait object rather than an `impl Trait` so the return type
/// does not change per platform: a caller stores it in one field.
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
