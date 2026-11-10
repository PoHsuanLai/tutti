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

use crate::InputProducerHandle;
use crate::capability::EndpointInfo;
use crate::devices::ChangeSink;
use crate::endpoints::{InputConnection, MidiEndpoints, NoWatch, Watch};
use crate::error::{Error, Result};
use tutti_midi_device::EndpointId;
use tutti_midi_runtime::MidiOut;

/// Why every call here fails, in the error a caller will surface.
const REASON: &str = "no native-UMP MIDI backend on this platform (Windows, or alsa-lib < 1.2.10)";

/// A [`MidiEndpoints`] that has none.
#[derive(Debug, Default)]
pub struct StubEndpoints;

impl StubEndpoints {
    /// The stub backend. Stateless — this is `Default` under another name, kept
    /// so every backend is constructed the same way.
    pub fn new() -> Self {
        Self
    }
}

impl MidiEndpoints for StubEndpoints {
    fn watch(&self, _sink: ChangeSink) -> Result<Box<dyn Watch>> {
        Ok(Box::new(NoWatch))
    }

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
mod tests;
