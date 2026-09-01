//! [`MidiGroup`] — which of a UMP Endpoint's 16 virtual cables a packet travels.
//!
//! An *address*, not a measure — so like [`MidiChannel`](super::MidiChannel) it
//! is an integer newtype rather than one of the float-backed units in
//! [`super::units`], and it deliberately does not implement
//! [`Unit`](super::Unit): a group is not an automatable DSP parameter and must
//! not be reachable through [`Param`](super::Param).
//!
//! # Group is transport addressing, Channel is voice addressing
//!
//! The two are four bits each and sit eight bits apart in the same UMP word,
//! which is exactly why they need separate types — nothing about their shape
//! distinguishes them, only their meaning does.
//!
//! Per MIDI 2.0 (M2-104-UM §2.1.2), the **Group** field is present in *every*
//! UMP packet header. It identifies one of the 16 virtual cables multiplexed
//! onto a single UMP Endpoint, and it is carried by messages that have no
//! channel at all: System Real-Time, SysEx7/SysEx8, Flex Data, and UMP Stream.
//! A group is the transport lane — the moral equivalent of "which of the 16
//! DIN cables this byte came out of".
//!
//! The **Channel** field, by contrast, exists only on Channel Voice messages
//! (UMP types 0x2 and 0x4) and names one of the 16 voice addresses *within* a
//! group. So the containment is real and one-directional: a group holds 16
//! channels, and `MidiChannel::COUNT == MidiGroup::COUNT == 16` is a
//! coincidence of field width, not a shared concept.
//!
//! # Why a newtype and not `u8`
//!
//! Every UMP channel-voice constructor takes `(group, channel, note, …)` —
//! adjacent `u8`s with no positional guard. Transposing the first two compiles,
//! does not crash, and produces a well-formed packet on the wrong cable
//! addressing the wrong voice. `channel_voice.rs` `debug_assert`s the widths,
//! but both fields are 4 bits, so a transposition of two in-range values passes
//! every assertion and is invisible in release. Two distinct types make it a
//! compile error:
//!
//! ```compile_fail
//! # use tutti_types::{MidiChannel, MidiGroup};
//! fn takes(_group: MidiGroup, _channel: MidiChannel) {}
//! // Transposed — the compiler rejects it, which is the entire point.
//! takes(MidiChannel::new(3), MidiGroup::new(1));
//! ```
//!
//! It lives here rather than in `tutti-midi-types` beside the constructors for
//! the same reason [`MidiChannel`](super::MidiChannel) does: a document has to
//! persist it, and `tutti-midi-types` carries no serde.

/// One of the 16 groups (virtual cables) in a UMP Endpoint.
///
/// Always in `0..=15`: [`new`](Self::new) masks rather than rejects, matching
/// what the UMP constructors already do with `group & 0x0F`.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
// `transparent` for the same reason `MidiChannel` has it: a `MidiGroup` is `3`
// on the wire, not `[3]`.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct MidiGroup(u8);

impl MidiGroup {
    /// The first group — and the only one a single-cable endpoint uses.
    ///
    /// This is the overwhelmingly common value: an engine that is not
    /// multiplexing several virtual cables onto one endpoint puts everything
    /// here, which is why [`Default`] resolves to it.
    pub const FIRST: MidiGroup = MidiGroup(0);
    /// The last group.
    pub const LAST: MidiGroup = MidiGroup(15);
    /// How many groups a UMP Endpoint carries.
    pub const COUNT: u8 = 16;

    /// Wrap a raw group number, masking into `0..=15`.
    ///
    /// Masks rather than returning an error because that is what the wire does:
    /// the UMP field is 4 bits, so a larger value cannot be represented and
    /// `channel_voice.rs` already `debug_assert`s then masks. Rejecting here
    /// would make this type stricter than the format it addresses.
    #[inline]
    pub const fn new(raw: u8) -> MidiGroup {
        MidiGroup(raw & 0x0F)
    }

    /// The raw 0-based group number, for a UMP constructor.
    #[inline]
    pub const fn get(self) -> u8 {
        self.0
    }
}

// Deliberately no `as_display_number`. `MidiChannel` has one because the
// 1-based channel numbering on hardware panels and in every DAW UI is a
// universal convention; MIDI 2.0 fixes no such convention for groups, and the
// spec numbers them 0-15 throughout. Inventing a `+ 1` here would put a
// display decision in the engine that no display has asked for. If a UI wants
// 1-based groups, it can add the one at its own edge and name it there.

impl From<MidiGroup> for u8 {
    #[inline]
    fn from(g: MidiGroup) -> u8 {
        g.0
    }
}

// Deliberately no `From<u8> for MidiGroup`, and no conversion to or from
// `MidiChannel` in either direction. The `u8` case is `MidiChannel`'s
// reasoning — the conversion masks, and a lossy conversion inside a trait that
// promises not to lose data is what `ChannelLayout` refused a reverse `From`
// for. The `MidiChannel` case is stronger: a total, lossless `From` between
// them would compile, and would silently reinstate exactly the transposition
// this type exists to reject.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_group_is_masked_into_range() {
        assert_eq!(MidiGroup::new(0).get(), 0);
        assert_eq!(MidiGroup::new(15).get(), 15);
        // 16 wraps to 0 — the same thing the UMP constructors do, rather than a
        // silently different answer one layer up.
        assert_eq!(MidiGroup::new(16).get(), 0);
        assert_eq!(MidiGroup::new(255).get(), 15);
    }

    #[test]
    fn a_group_and_a_channel_are_indistinguishable_by_value() {
        use crate::MidiChannel;
        // This is the premise that makes the separate type necessary, not a
        // property of it: the two fields have identical width, identical
        // masking, and identical range, so *nothing in the value* can tell a
        // transposed argument from a correct one. Only the type system can.
        // The compile-time half of this is the `compile_fail` doctest in the
        // module docs above, which a `#[test]` cannot express.
        assert_eq!(MidiGroup::new(7).get(), MidiChannel::new(7).get());
        assert_eq!(MidiGroup::new(16).get(), MidiChannel::new(16).get());
        assert_eq!(MidiGroup::COUNT, MidiChannel::COUNT);
    }
}
