//! A **native-UMP** virtual MIDI source (macOS / CoreMIDI).
//!
//! The safe `coremidi` wrapper only exposes a MIDI-1.0 virtual source
//! (`MIDISourceCreate`), whose only send path is legacy packet bytes, which
//! carries MIDI 1.0 messages only, so a MIDI-2-only message
//! (per-note controllers, **and JR Timestamps**) cannot reach the wire through it. Publishing
//! a genuine UMP stream, the one transport where a JR Timestamp actually
//! reaches the wire, needs a MIDI-2.0-protocol endpoint. The raw CoreMIDI calls
//! for it are `tutti-coremidi-ffi`'s (`UmpSource`); this module names the
//! endpoint and puts its errors in this crate's.
//!
//! [`UmpVirtualSource`] creates one such endpoint and sends `&[u32]` UMP words
//! directly. It owns the endpoint, disposed on drop.

use tracing::debug;
use tutti_coremidi_ffi::UmpSource;

use crate::error::Result;

/// A CoreMIDI virtual source that speaks the **MIDI 2.0 (UMP) protocol**.
///
/// Unlike a MIDI-1.0 virtual source (byte packets, which carry MIDI 1.0
/// messages only), this endpoint carries UMP
/// words end to end, so JR Timestamps and other MIDI-2-only messages survive to
/// the wire. Owns its endpoint, which is disposed on drop.
#[must_use = "dropping it removes the virtual source"]
pub struct UmpVirtualSource {
    source: UmpSource,
    name: String,
}

impl core::fmt::Debug for UmpVirtualSource {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Owns an opaque CoreMIDI handle; report the endpoint name only.
        f.debug_struct("UmpVirtualSource")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl UmpVirtualSource {
    /// Creates a MIDI-2.0 virtual source named `name`, visible to other apps as a
    /// UMP-capable MIDI source.
    ///
    /// Keep the value alive: dropping it disposes the endpoint.
    ///
    /// # Errors
    ///
    /// [`Error::CoreMidi`](crate::Error::CoreMidi) if CoreMIDI refuses either the
    /// client or the endpoint.
    pub fn new(name: &str) -> Result<Self> {
        // CoreMIDI sends notifications to the run loop of the process's first
        // client, so the device watch's client starts before the source's own
        // (see `hub_first`).
        super::hub_first();
        let source = UmpSource::new(name)?;
        debug!(name, "Created native-UMP virtual MIDI source");
        Ok(Self {
            source,
            name: name.to_string(),
        })
    }

    /// Publish one UMP message (1-4 words) to the endpoint's subscribers.
    ///
    /// `words` is a complete UMP message; its length must match the message type
    /// (a JR Timestamp is 1 word, a note is 1-2, SysEx8 is 4). Empty input is a
    /// no-op. CoreMIDI copies the words, so no lifetime escapes this call.
    ///
    /// # Errors
    ///
    /// [`Error::CoreMidi`](crate::Error::CoreMidi) when the message will not fit
    /// the event list, or when `MIDIReceivedEventList` returns a non-zero
    /// `OSStatus`.
    pub fn send_ump(&self, words: &[u32]) -> Result<()> {
        Ok(self.source.send(words)?)
    }

    /// The source's display name.
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for UmpVirtualSource {
    fn drop(&mut self) {
        debug!(name = %self.name, "Dropping native-UMP virtual MIDI source");
    }
}

#[cfg(test)]
mod tests;
