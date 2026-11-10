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
//! [`Protocol`] is the MIDI 2.0 spec's own noun from `tutti-midi-types`, reused
//! here rather than restated. The endpoint's Function Blocks are not here: the
//! `Endpoint` model of design 017 §5 (stage 4b) will hold them.

use tutti_midi_device::{EndpointId, Identity};
use tutti_midi_types::Protocol;

/// What an endpoint can carry.
///
/// Built by the backend at enumeration time from whatever the OS reports, and
/// read by anything deciding how to talk to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UmpCapability {
    /// The protocol the endpoint speaks.
    ///
    /// `Midi1` is the common case even on a UMP-native transport: most hardware
    /// in the world is MIDI 1.0, and both CoreMIDI and the ALSA kernel converter
    /// will happily hand us UMP words that *originated* as MIDI 1.0. That is a
    /// real distinction a caller may act on (a per-note controller is pointless
    /// to send to a `Midi1` endpoint), so it is recorded rather than flattened.
    pub protocol: Protocol,
}

/// Hand-written rather than derived, and the polarity is the point.
///
/// `Protocol`'s own `Default` is `Midi2` — correct for this engine, which is
/// MIDI-2-native, and the **wrong** default for a *device* whose protocol the OS
/// never told us. Deriving would inherit that and quietly claim MIDI 2.0 for
/// every unknown endpoint, sending per-note messages into a `to_midi1_bytes`
/// drop. Assuming MIDI 1.0 only costs resolution, so unknown means `Midi1`.
impl Default for UmpCapability {
    fn default() -> Self {
        Self::midi1()
    }
}

impl UmpCapability {
    /// A MIDI-2.0 endpoint.
    pub fn midi2() -> Self {
        Self {
            protocol: Protocol::Midi2,
        }
    }

    /// A MIDI-1.0 endpoint.
    pub fn midi1() -> Self {
        Self {
            protocol: Protocol::Midi1,
        }
    }

    /// Whether this endpoint carries MIDI-2-only messages — per-note
    /// controllers, per-note pitch bend, and JR Timestamps — intact.
    ///
    /// A `Midi1` endpoint reaches the wire through `to_midi1_bytes`, which
    /// returns `None` for exactly those, so they are dropped. This is the
    /// question a caller actually asks, named once so no call site re-derives it
    /// from `protocol` and gets the polarity wrong.
    pub fn carries_midi2_only(&self) -> bool {
        matches!(self.protocol, Protocol::Midi2)
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
