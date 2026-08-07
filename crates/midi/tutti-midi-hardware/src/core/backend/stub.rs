//! The no-backend backend.
//!
//! Used where no native-UMP path exists: Windows (no `windows` crate version
//! binds Windows MIDI Services; WinRT `Devices.Midi` is MIDI-1.0 message types)
//! and a Linux built against alsa-lib < 1.2.10.
//!
//! # It reports "unsupported", not "no devices"
//!
//! Enumeration returns an empty list — there genuinely are none — but *opening*
//! returns [`Error::Unsupported`] rather than a not-found error. The distinction
//! matters to whoever renders it: "no MIDI devices" tells a Windows user to
//! check their cables, when the truth is that this build has no MIDI backend at
//! all. One is a hardware problem, the other is a build fact.
//!
//! # Deliberately trivial
//!
//! No dependencies, no `unsafe`, no state, every method a one-liner. This is the
//! one backend that cannot be compiled on the machine most of this crate is
//! developed on, so it is written to be verifiable by reading. **If it ever
//! needs logic, that is a signal to reconsider rather than to grow it.**

use crate::core::capability::{EndpointId, EndpointInfo};
use crate::core::endpoints::{InputConnection, MidiEndpoints};
use crate::core::error::{Error, Result};
use crate::core::InputProducerHandle;
use tutti_midi_types::MidiOut;

/// Why every call here fails, in the error a caller will surface.
const REASON: &str = "no native-UMP MIDI backend on this platform (Windows, or alsa-lib < 1.2.10)";

/// A [`MidiEndpoints`] that has none.
#[derive(Debug, Default)]
pub struct StubEndpoints;

impl StubEndpoints {
    pub fn new() -> Self {
        Self
    }
}

impl MidiEndpoints for StubEndpoints {
    fn inputs(&self) -> Vec<EndpointInfo> {
        Vec::new()
    }

    fn outputs(&self) -> Vec<EndpointInfo> {
        Vec::new()
    }

    fn open_input(
        &self,
        _id: EndpointId,
        _producer: InputProducerHandle,
    ) -> Result<Box<dyn InputConnection>> {
        Err(Error::Unsupported(REASON))
    }

    fn open_output(&self, _id: EndpointId) -> Result<Box<dyn MidiOut>> {
        Err(Error::Unsupported(REASON))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_enumerates_nothing() {
        let b = StubEndpoints::new();
        assert!(b.inputs().is_empty());
        assert!(b.outputs().is_empty());
    }

    /// Opening must be `Unsupported`, not a not-found error — the caller renders
    /// these differently, and conflating them hides a build fact behind a
    /// hardware one.
    #[test]
    fn opening_reports_unsupported_rather_than_not_found() {
        let b = StubEndpoints::new();
        assert!(matches!(
            b.open_output(EndpointId::from_raw(0)),
            Err(Error::Unsupported(_))
        ));
    }
}
