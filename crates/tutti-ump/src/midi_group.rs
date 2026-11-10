//! [`MidiGroup`] — which of a UMP Endpoint's 16 virtual cables a packet travels.
//!
//! An *address*, not a measure — so like [`MidiChannel`](crate::MidiChannel) it
//! is an integer newtype rather than one of the float-backed units in
//! `tutti_types`, and it deliberately does not implement
//! `Unit`: a group is not an automatable DSP parameter and must
//! not be reachable through `Param`.
//!
//! # Group is transport addressing, Channel is voice addressing
//!
//! The two are four bits each and sit eight bits apart in the same UMP word,
//! which is exactly why they need separate types — nothing about their shape
//! distinguishes them, only their meaning does.
//!
//! Per MIDI 2.0 (M2-104-UM §2.1.2), the **Group** field is present in every
//! UMP packet except Utility (type `0x0`) and Stream (type `0xF`), which have
//! none. It identifies one of the 16 virtual cables multiplexed
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
//! addressing the wrong voice. Both fields are 4 bits, so a transposition of
//! two in-range values passes every width check. Two distinct types make it a
//! compile error:
//!
//! ```compile_fail
//! # use tutti_ump::{MidiChannel, MidiGroup};
//! fn takes(_group: MidiGroup, _channel: MidiChannel) {}
//! // Transposed — the compiler rejects it, which is the entire point.
//! takes(MidiChannel::of(3), MidiGroup::of(1));
//! ```
//!
//! Like [`MidiChannel`](crate::MidiChannel), it is serializable (feature `serde`).

use crate::OutOfRange;

/// One of the 16 groups (virtual cables) in a UMP Endpoint.
///
/// Always in `0..=15`: [`new`](Self::new) refuses a larger number, and
/// [`masked`](Self::masked) is the one constructor that masks.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
// Serialized as the bare number, and read back through `TryFrom<u8>`, so a
// document cannot carry a value the type refuses.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "u8", into = "u8"))]
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

    /// The group, or `None` when `raw` is above 15.
    ///
    /// ```
    /// use tutti_ump::MidiGroup;
    ///
    /// assert_eq!(MidiGroup::new(15), Some(MidiGroup::LAST));
    /// assert_eq!(MidiGroup::new(16), None);
    /// ```
    #[inline]
    #[must_use]
    pub const fn new(raw: u8) -> Option<MidiGroup> {
        if raw < Self::COUNT {
            Some(MidiGroup(raw))
        } else {
            None
        }
    }

    /// The group for a literal or a constant. For a value that arrives from
    /// outside, use [`new`](Self::new).
    ///
    /// # Panics
    ///
    /// When `raw` is above 15; in a `const` that is a compile error.
    #[inline]
    #[must_use]
    #[track_caller]
    pub const fn of(raw: u8) -> MidiGroup {
        match Self::new(raw) {
            Some(group) => group,
            None => panic!("a group is 0 to 15"),
        }
    }

    /// The low four bits of `raw`: the one place that masks. Use it where the
    /// bits are known to be a Group field, as the decoder does; a value from
    /// outside the wire is checked with [`new`](Self::new).
    #[inline]
    #[must_use]
    pub const fn masked(raw: u8) -> MidiGroup {
        MidiGroup(raw & 0x0F)
    }

    /// Returns the raw 0-based group number, for a UMP constructor.
    #[inline]
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

// Deliberately no `display_number`. `MidiChannel` has one because the
// 1-based channel numbering on hardware panels and in every DAW UI is a
// universal convention; MIDI 2.0 fixes no such convention for groups, and the
// spec numbers them 0-15 throughout. Inventing a `+ 1` here would put a
// display decision in the engine that no display has asked for. If a UI wants
// 1-based groups, it can add the one at its own edge and name it there.

impl TryFrom<u8> for MidiGroup {
    type Error = OutOfRange;

    #[inline]
    fn try_from(raw: u8) -> Result<Self, OutOfRange> {
        Self::new(raw).ok_or(OutOfRange {
            what: "group",
            value: u32::from(raw),
            max: u32::from(Self::LAST.0),
        })
    }
}

impl From<MidiGroup> for u8 {
    #[inline]
    fn from(g: MidiGroup) -> u8 {
        g.0
    }
}

// Deliberately no infallible `From<u8> for MidiGroup`, and no conversion to or from
// `MidiChannel` in either direction. The `u8` case is `MidiChannel`'s
// reasoning: the conversion would mask or fail, and a lossy conversion inside
// a trait that promises not to lose data is what `ChannelLayout` refused a
// reverse `From` for. The `MidiChannel` case is stronger: a total, lossless `From` between
// them would compile, and would silently reinstate exactly the transposition
// this type exists to reject.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_from_agrees_with_new() {
        for raw in 0..=255u8 {
            assert_eq!(MidiGroup::try_from(raw).ok(), MidiGroup::new(raw));
        }
        assert_eq!(
            MidiGroup::try_from(16),
            Err(OutOfRange {
                what: "group",
                value: 16,
                max: 15
            })
        );
    }

    #[test]
    fn a_group_out_of_range_is_refused() {
        for raw in 0..=255u8 {
            match MidiGroup::new(raw) {
                Some(group) => {
                    assert!(raw < 16);
                    assert_eq!(group.get(), raw);
                    assert_eq!(MidiGroup::of(raw), group);
                }
                None => assert!(raw >= 16),
            }
        }
    }

    #[test]
    fn masking_is_only_through_masked() {
        assert_eq!(MidiGroup::masked(16).get(), 0);
        assert_eq!(MidiGroup::masked(255).get(), 15);
        assert_eq!(MidiGroup::masked(7), MidiGroup::of(7));
    }

    #[test]
    #[should_panic(expected = "a group is 0 to 15")]
    fn of_panics_out_of_range() {
        let _ = MidiGroup::of(16);
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
        assert_eq!(MidiGroup::of(7).get(), MidiChannel::of(7).get());
        assert_eq!(MidiGroup::new(16), None);
        assert_eq!(MidiChannel::new(16), None);
        assert_eq!(MidiGroup::COUNT, MidiChannel::COUNT);
    }
}
