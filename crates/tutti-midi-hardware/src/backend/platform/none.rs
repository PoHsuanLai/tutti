//! A platform with no native backend: Windows, or Linux with an alsa-lib older
//! than the UMP sequencer API. The stub is its backend.

use crate::endpoints::MidiEndpoints;

/// Returns this platform's MIDI backend: the stub.
pub(crate) fn active() -> Box<dyn MidiEndpoints> {
    Box::new(super::stub::StubEndpoints::new())
}
