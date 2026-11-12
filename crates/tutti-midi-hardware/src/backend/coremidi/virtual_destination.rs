//! A **native-UMP** virtual MIDI destination (macOS / CoreMIDI) — the inbound
//! counterpart to [`UmpVirtualSource`](super::UmpVirtualSource).
//!
//! Why this exists: over a MIDI-1.0 transport every inbound message arrives as
//! legacy bytes. Most of MIDI 2.0 survives that — MIDI-CI is Universal SysEx *by
//! design* (M2-101), precisely so two devices can negotiate before either knows
//! the other speaks MIDI 2.0 — but **UMP Stream** (message type 0xF: Endpoint
//! Discovery / Info / Function Block, M2-104 §7.1) has *no* MIDI-1.0 encoding at
//! all. It can only arrive over a MIDI-2.0-protocol endpoint, which is what this
//! publishes.
//!
//! [`UmpVirtualDestination`] creates one such endpoint and hands every received
//! UMP message to a callback. The endpoint and its receive block are
//! `tutti-coremidi-ffi`'s (`UmpDestination`): the safe `coremidi` wrapper's
//! `virtual_destination_with_protocol` reads the null reference constant
//! CoreMIDI passes a virtual destination as a reference, and so aborts on the
//! first packet. This module names the endpoint and puts its errors in this
//! crate's.
//!
//! CoreMIDI delivers a packet as a *packed word stream* — several concatenated
//! UMP messages with no separators, each one's length implied by its type nibble.
//! [`split_stream`] walks that.

use tracing::debug;
use tutti_coremidi_ffi::UmpDestination;
use tutti_ump::{Ump, split_stream};

use crate::error::Result;

/// A CoreMIDI virtual destination that speaks the **MIDI 2.0 (UMP) protocol**.
///
/// Other apps see it as a UMP-capable MIDI destination and can send us native
/// UMP — including the UMP-Stream messages that have no MIDI-1.0 form. Each
/// received message is passed to the callback supplied at construction, on
/// CoreMIDI's own delivery thread.
///
/// Keep the value alive: dropping it tears the endpoint down.
pub struct UmpVirtualDestination {
    // Retained purely to own the endpoint's lifetime — the callback installed at
    // creation is what actually does the work.
    _destination: UmpDestination,
    name: String,
}

impl core::fmt::Debug for UmpVirtualDestination {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UmpVirtualDestination")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl UmpVirtualDestination {
    /// Creates a MIDI-2.0 virtual destination named `name`, invoking `on_event`
    /// for every UMP message received.
    ///
    /// `on_event` runs on CoreMIDI's delivery thread, so it must not block —
    /// push into a lock-free ring (see
    /// [`with_producer`](Self::with_producer)) rather than doing work inline.
    ///
    /// # Errors
    ///
    /// [`Error::CoreMidi`](crate::Error::CoreMidi) if CoreMIDI refuses either
    /// the client or the destination endpoint.
    pub fn new<F>(name: &str, mut on_event: F) -> Result<Self>
    where
        F: FnMut(Ump) + Send + 'static,
    {
        Self::stamped(name, move |event, _arrived| on_event(event))
    }

    /// As [`new`](Self::new), also giving each message the `Instant` its packet
    /// reached CoreMIDI (the packet's host time).
    fn stamped<F>(name: &str, mut on_event: F) -> Result<Self>
    where
        F: FnMut(Ump, std::time::Instant) + Send + 'static,
    {
        let destination = UmpDestination::new(name, move |timestamp, words| {
            let arrived = super::host_time::Pairing::now().instant_of(timestamp);
            // One packet carries several concatenated UMP messages.
            for event in split_stream(words) {
                on_event(event, arrived);
            }
        })?;

        debug!(name, "Created native-UMP virtual MIDI destination");
        Ok(Self {
            _destination: destination,
            name: name.to_string(),
        })
    }

    /// Creates a MIDI-2.0 virtual destination that pushes every received message
    /// into an input ring, exactly like a hardware input port does.
    ///
    /// This is the wiring an app wants: the events land in the same
    /// [`HardwareMidiInputs`](crate::HardwareMidiInputs) rings a physical
    /// endpoint feeds, so downstream consumers see one merged stream regardless
    /// of which transport a message arrived on.
    ///
    /// # Errors
    ///
    /// As [`new`](Self::new).
    pub fn with_producer(name: &str, mut producer: crate::InputProducerHandle) -> Result<Self> {
        Self::stamped(name, move |event, arrived| {
            if producer.push(event, arrived) == crate::Pushed::Full {
                debug!("UMP input ring full, dropping event");
            }
        })
    }

    /// The destination's display name.
    pub fn name(&self) -> &str {
        &self.name
    }
}

#[cfg(test)]
mod tests;
