//! What a MIDI endpoint is and what it can carry.
//!
//! [`EndpointInfo`] is what enumeration returns: a name, an opaque
//! [`EndpointId`] to open it by, and a [`UmpCapability`] describing what the OS
//! says it can do.
//!
//! # Why capability is a value, not a type parameter or a `cfg`
//!
//! The thing this crate exists to fix is a `#[cfg]` ladder that chose a
//! *transport* by platform, so that MIDI-2-only messages survived on macOS and
//! were silently dropped everywhere else. Capability is a property of the
//! **endpoint**, not of the code: two devices on the same machine, through the
//! same backend, can differ. Encoding it in the type or behind a `cfg` is what
//! made the old split unrepresentable-in-the-right-place and invisible in the
//! wrong one.
//!
//! So it is a plain value that travels with the endpoint, and the dispatch that
//! reads it is an ordinary `match`.
//!
//! # It reuses the engine's vocabulary rather than restating it
//!
//! [`Protocol`] and [`FunctionBlock`] are already the MIDI 2.0 spec's own nouns
//! in `tutti-midi-types` / `tutti-midi-runtime`, and they are exactly the shape
//! the OS hands back — ALSA's `snd_ump_block_info_get_{direction,first_group,
//! num_groups,name}` populates a `FunctionBlock` field for field. Re-declaring
//! them here would be two names for one behaviour, which the units rule already
//! rejects.

use tutti_midi_runtime::FunctionBlock;
use tutti_midi_types::Protocol;

/// A handle to one endpoint, opaque and backend-owned.
///
/// Deliberately **not** a bare index. CoreMIDI's `MIDIGetSource(i)` and ALSA's
/// `(client, port)` pair are different shapes, and an index is only stable until
/// the next hot-plug — the value that survives a device list refresh differs per
/// OS. Keeping it opaque means a stale id fails to open rather than silently
/// opening the *wrong device*, which is the failure a `usize` invites.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EndpointId {
    /// Backend-defined. CoreMIDI stores a `MIDIUniqueID`; ALSA packs
    /// `(client << 8) | port`.
    raw: u64,
}

impl EndpointId {
    /// Mint an id from a backend's own stable identifier.
    pub fn from_raw(raw: u64) -> Self {
        Self { raw }
    }

    /// The backend's identifier. Meaningful only to the backend that minted it.
    pub fn raw(self) -> u64 {
        self.raw
    }
}

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

    /// The endpoint's function blocks, if it declared any.
    ///
    /// Empty is the honest answer for "the OS did not say", which is distinct
    /// from "the device has none" — and neither is worth inventing a block for.
    /// ALSA fills these from `snd_ump_block_info_*`; CoreMIDI has no equivalent
    /// today, so it reports none.
    pub function_blocks: Vec<FunctionBlock>,
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
    /// A MIDI-2.0 endpoint with no declared function blocks.
    pub fn midi2() -> Self {
        Self {
            protocol: Protocol::Midi2,
            function_blocks: Vec::new(),
        }
    }

    /// A MIDI-1.0 endpoint with no declared function blocks.
    pub fn midi1() -> Self {
        Self {
            protocol: Protocol::Midi1,
            function_blocks: Vec::new(),
        }
    }

    /// Attach function blocks (consuming builder, as [`EndpointNegotiator`]
    /// already does for the protocol-level equivalent).
    ///
    /// [`EndpointNegotiator`]: tutti_midi_runtime::EndpointNegotiator
    pub fn with_function_blocks(mut self, blocks: Vec<FunctionBlock>) -> Self {
        self.function_blocks = blocks;
        self
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
    /// Display name, as the OS reports it.
    pub name: String,
    /// What it can carry.
    pub capability: UmpCapability,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_midi_types::FunctionBlockDirection;
    use tutti_midi_types::MidiGroup;

    #[test]
    fn a_midi1_endpoint_does_not_carry_midi2_only_messages() {
        assert!(!UmpCapability::midi1().carries_midi2_only());
        assert!(UmpCapability::midi2().carries_midi2_only());
    }

    /// The default must be the *conservative* one.
    ///
    /// `Protocol`'s own `Default` is `Midi2` because the engine is MIDI-2-native
    /// — correct there, and the wrong polarity for a *device* whose protocol the
    /// OS never reported. Claiming MIDI 2.0 for an unknown endpoint would send
    /// per-note messages into a `to_midi1_bytes` drop; claiming MIDI 1.0 only
    /// costs resolution. This pins which way that falls.
    #[test]
    fn an_unreported_capability_defaults_to_midi1() {
        assert_eq!(UmpCapability::default().protocol, Protocol::Midi1);
        assert!(!UmpCapability::default().carries_midi2_only());
    }

    #[test]
    fn function_blocks_are_absent_until_declared() {
        let cap = UmpCapability::midi2();
        assert!(cap.function_blocks.is_empty());

        let with = cap.with_function_blocks(vec![FunctionBlock {
            block_number: 0,
            first_group: MidiGroup::FIRST,
            num_groups: 1,
            direction: FunctionBlockDirection::Bidirectional,
            name: "Main".to_string(),
        }]);
        assert_eq!(with.function_blocks.len(), 1);
        assert_eq!(with.function_blocks[0].name, "Main");
    }

    /// A stale id must not resolve to a different device. Two endpoints minted
    /// from different raws are never equal, so a backend that looks up by id
    /// fails to find a departed device rather than matching whatever now sits at
    /// that index.
    #[test]
    fn endpoint_ids_are_distinct_per_raw() {
        assert_ne!(EndpointId::from_raw(1), EndpointId::from_raw(2));
        assert_eq!(EndpointId::from_raw(7).raw(), 7);
    }
}
