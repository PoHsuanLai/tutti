//! What a MIDI endpoint is and what it can carry.
//!
//! [`EndpointInfo`] is what enumeration returns: a name, an opaque
//! [`EndpointId`] to open it by, and a [`UmpCapability`] describing what the OS
//! says it can do.
//!
//! Capability is a property of the **endpoint**, not of the platform: two
//! devices on the same machine, through the same backend, can differ. So it is
//! a plain value that travels with the endpoint, read from the OS rather than
//! assumed, and code that depends on it is an ordinary `match`.
//!
//! [`Protocol`] is the MIDI 2.0 spec's own noun from `tutti-ump`, reused
//! here rather than restated; the blocks are `tutti-midi-device`'s `FunctionBlock`.

use tutti_midi_device::{Capabilities, Endpoint, EndpointId, FunctionBlock, Identity};
use tutti_ump::{Protocol, StreamConfig};

/// What an endpoint can carry, and the blocks it declares.
///
/// Built by the backend at enumeration time from whatever the OS reports (an
/// [`Endpoint`] the OS describes), and after a negotiation from the
/// [`Endpoint`] the device itself described ([`DeviceStream::capability`]).
/// Read by anything deciding how to talk to it.
///
/// [`DeviceStream::capability`]: crate::DeviceStream::capability
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UmpCapability {
    /// The protocol the endpoint speaks now.
    ///
    /// `Midi1` is the common case even on a UMP-native transport: most hardware
    /// in the world is MIDI 1.0, and both CoreMIDI and the ALSA kernel converter
    /// will happily hand us UMP words that *originated* as MIDI 1.0. That is a
    /// real distinction a caller may act on (a per-note controller is pointless
    /// to send to a `Midi1` endpoint), so it is recorded rather than flattened.
    pub protocol: Protocol,
    /// The Function Blocks the endpoint declares (the kernel's Group Terminal
    /// Blocks), by number: which Groups each spans, in which direction, and its
    /// name. Empty where the OS exposes none (a MIDI 1.0 port, or a backend
    /// that does not read them).
    pub blocks: Vec<FunctionBlock>,
}

/// Hand-written rather than derived, and the polarity is the point.
///
/// MIDI 2.0 is what this engine speaks, and the **wrong** default for a *device*
/// whose protocol the OS never told us: it would quietly claim MIDI 2.0 for
/// every unknown endpoint, sending it per-note messages a MIDI 1.0 endpoint
/// cannot carry. Assuming MIDI 1.0 only costs resolution, so unknown means `Midi1`.
impl Default for UmpCapability {
    fn default() -> Self {
        Self::midi1()
    }
}

impl UmpCapability {
    /// A MIDI-2.0 endpoint that declares no blocks.
    pub fn midi2() -> Self {
        Self {
            protocol: Protocol::Midi2,
            blocks: Vec::new(),
        }
    }

    /// A MIDI-1.0 endpoint that declares no blocks.
    pub fn midi1() -> Self {
        Self {
            protocol: Protocol::Midi1,
            blocks: Vec::new(),
        }
    }

    /// What `endpoint` is: the protocol it has active, and its blocks. A
    /// reserved protocol is one nobody can speak, so it reads as MIDI 1.0.
    #[must_use]
    pub fn of(endpoint: &Endpoint) -> Self {
        Self {
            protocol: usable(endpoint.active().protocol),
            blocks: endpoint.function_blocks().copied().collect(),
        }
    }

    /// The capability with `blocks` declared.
    #[must_use]
    pub fn with_blocks(mut self, blocks: Vec<FunctionBlock>) -> Self {
        self.blocks = blocks;
        self
    }

    /// Whether this endpoint carries MIDI-2-only messages — per-note
    /// controllers, per-note pitch bend, and JR Timestamps — intact.
    ///
    /// A `Midi1` endpoint carries MIDI 1.0 messages, which have none of those:
    /// the output narrows the graph's MIDI 2.0 messages for it (`OpenOutput`,
    /// design 017 L6(c)).
    /// This is the question a caller actually asks, named once so no call site
    /// re-derives it from `protocol` and gets the polarity wrong.
    pub fn carries_midi2_only(&self) -> bool {
        matches!(self.protocol, Protocol::Midi2)
    }

    /// Whether the endpoint is a UMP Endpoint that can be asked what it is:
    /// one in the MIDI 2.0 Protocol, or one that declares blocks. A MIDI 1.0
    /// port with no blocks has no UMP Stream to ask (M2-104 Appendix I).
    pub(crate) fn is_ump_endpoint(&self) -> bool {
        self.carries_midi2_only() || !self.blocks.is_empty()
    }

    /// The Endpoint as the OS describes it: in the protocol it reports, with
    /// JR Timestamps off (the OS reports none), and the blocks it declares. A
    /// MIDI 1.0 port that declares none is [`Endpoint::midi1_device`].
    pub(crate) fn endpoint(&self) -> Endpoint {
        let protocol = usable(self.protocol);
        let mut endpoint = if protocol == Protocol::Midi1 && self.blocks.is_empty() {
            Endpoint::midi1_device(None)
        } else {
            Endpoint::new(Capabilities {
                midi1: protocol == Protocol::Midi1,
                midi2: protocol == Protocol::Midi2,
                rx_jr: false,
                tx_jr: false,
            })
        };
        endpoint = endpoint.with_active(StreamConfig {
            protocol,
            rx_jr: false,
            tx_jr: false,
        });
        for block in &self.blocks {
            endpoint = endpoint.with_function_block(*block);
        }
        endpoint
    }
}

/// A protocol this engine can speak: a reserved value reads as MIDI 1.0.
fn usable(protocol: Protocol) -> Protocol {
    match protocol {
        Protocol::Reserved(_) => Protocol::Midi1,
        p => p,
    }
}

/// One endpoint as enumeration found it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndpointInfo {
    /// Opaque handle to open it by.
    pub id: EndpointId,
    /// What the OS knows it by, for a [`DeviceId`](tutti_midi_device::DeviceId).
    pub identity: Identity,
    /// Display name, as the OS reports it.
    pub name: String,
    /// What it can carry.
    pub capability: UmpCapability,
}

#[cfg(test)]
mod tests;
