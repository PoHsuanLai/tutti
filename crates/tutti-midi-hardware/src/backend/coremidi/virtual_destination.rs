//! A virtual MIDI destination on CoreMIDI, in the UMP protocol the spec asks
//! for: the inbound counterpart to [`UmpVirtualSource`](super::UmpVirtualSource).
//!
//! Over a MIDI-1.0 transport every inbound message arrives as legacy bytes.
//! Most of MIDI 2.0 survives that (MIDI-CI is Universal SysEx by design,
//! M2-101), but **UMP Stream** (message type 0xF, M2-104 §7.1) has no MIDI-1.0
//! encoding at all: it arrives only over a UMP endpoint, which is what this
//! publishes.
//!
//! [`UmpVirtualDestination`] hands every received UMP message to a callback.
//! The endpoint and its receive block are `tutti-coremidi-ffi`'s
//! (`UmpDestination`): the safe `coremidi` wrapper's
//! `virtual_destination_with_protocol` reads the null reference constant
//! CoreMIDI passes a virtual destination as a reference, and so aborts on the
//! first packet. This module names the endpoint and puts its errors in this
//! crate's.
//!
//! CoreMIDI delivers a packet as a *packed word stream*: several concatenated
//! UMP messages with no separators, each one's length implied by its type
//! nibble. [`split_stream`] walks that.

use tracing::debug;
use tutti_coremidi_ffi::UmpDestination;
use tutti_ump::{Ump, split_stream};

use super::virtual_source::protocol_of;
use crate::endpoints::InputConnection;
use crate::error::{Error, Result};
use crate::virtual_endpoint::{VirtualProtocol, VirtualSpec};

/// A CoreMIDI virtual destination that speaks the UMP protocol it was created
/// with.
///
/// Other apps see a UMP-capable destination and can send it native UMP,
/// including the UMP-Stream messages that have no MIDI-1.0 form. Each received
/// message is passed to the callback supplied at construction, on CoreMIDI's own
/// delivery thread. Dropping it tears the endpoint down.
#[must_use = "dropping it removes the virtual destination"]
pub struct UmpVirtualDestination {
    // Owns the endpoint's lifetime; the callback installed at creation does the
    // work.
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

/// Every message delivered is on CoreMIDI's thread, and the endpoint is
/// disposed by the drop: nothing to report beyond that it delivers.
impl InputConnection for UmpVirtualDestination {}

impl UmpVirtualDestination {
    /// Publishes the destination `spec` describes, pushing every received
    /// message into `producer` exactly as a hardware input port does: the
    /// events land in the same [`HardwareMidiInputs`](crate::HardwareMidiInputs)
    /// rings a physical endpoint feeds. The endpoint carries a name and a
    /// protocol: CoreMIDI's endpoint API takes no Function Blocks.
    ///
    /// # Errors
    ///
    /// [`Error::VirtualBlocksUnsupported`] when `spec` declares blocks, and
    /// [`Error::CoreMidi`] if CoreMIDI refuses the client or the endpoint.
    pub(crate) fn open(
        spec: &VirtualSpec,
        mut producer: crate::InputProducerHandle,
    ) -> Result<Self> {
        if !spec.function_blocks().is_empty() {
            return Err(Error::VirtualBlocksUnsupported);
        }
        Self::stamped(
            spec.name().as_str(),
            spec.protocol(),
            move |event, arrived| {
                if producer.push(event, arrived) == crate::Pushed::Full {
                    debug!("UMP input ring full, dropping event");
                }
            },
        )
    }

    /// Creates a virtual destination named `name` carrying `protocol`, invoking
    /// `on_event` for every UMP message received.
    ///
    /// `on_event` runs on CoreMIDI's delivery thread, so it must not block.
    ///
    /// # Errors
    ///
    /// [`Error::CoreMidi`] if CoreMIDI refuses either the client or the
    /// destination endpoint.
    #[cfg(test)]
    pub(crate) fn new<F>(name: &str, protocol: VirtualProtocol, mut on_event: F) -> Result<Self>
    where
        F: FnMut(Ump) + Send + 'static,
    {
        Self::stamped(name, protocol, move |event, _arrived| on_event(event))
    }

    /// As [`new`](Self::new), also giving each message the `Instant` its packet
    /// reached CoreMIDI (the packet's host time).
    fn stamped<F>(name: &str, protocol: VirtualProtocol, mut on_event: F) -> Result<Self>
    where
        F: FnMut(Ump, std::time::Instant) + Send + 'static,
    {
        let destination =
            UmpDestination::new(name, protocol_of(protocol), move |timestamp, words| {
                let arrived = super::host_time::Pairing::now().instant_of(timestamp);
                // One packet carries several concatenated UMP messages.
                for event in split_stream(words) {
                    on_event(event, arrived);
                }
            })?;

        debug!(name, ?protocol, "Created virtual MIDI destination");
        Ok(Self {
            _destination: destination,
            name: name.to_string(),
        })
    }
}

#[cfg(test)]
mod tests;
