//! A platform with no native backend: Windows, or Linux with an alsa-lib older
//! than the UMP sequencer API. The stub is its backend.

use crate::endpoints::MidiEndpoints;

/// Returns this platform's MIDI backend: the stub.
///
/// Returns a boxed trait object rather than an `impl Trait` so the return type
/// does not change per platform: a caller stores it in one field.
pub(crate) fn active() -> Box<dyn MidiEndpoints> {
    Box::new(super::stub::StubEndpoints::new())
}
