//! [`MidiChannel`] — which of a MIDI group's 16 channels a message addresses.
//!
//! An *address*, not a measure — so like [`Samples`](super::Samples) it is an
//! integer newtype rather than one of the float-backed units in
//! [`super::units`], and it deliberately does not implement
//! [`Unit`](super::Unit): a channel is not an automatable DSP parameter and must
//! not be reachable through [`Param`](super::Param).
//!
//! # Why a newtype and not `u8`
//!
//! Every UMP constructor takes `(group, channel, note, …)` — three adjacent
//! `u8`s. Nothing stops two of them being swapped, and the result is not a
//! crash: the note plays on the wrong channel, which under MPE means the wrong
//! *voice*, and per-note expression lands on a note nobody is holding.
//!
//! A `pub type MidiChannel = u8` alias prevents none of that — an alias is the
//! same type. This is the real newtype, and `tutti-midi-types` re-exports it.
//! It lives here rather than there because a document has to persist a channel
//! and that crate carries no serde.
//!
//! See [`MidiGroup`](super::MidiGroup) for the other half: typing a channel
//! alone still leaves `(group, channel)` transposable, since both are 4-bit and
//! both mask silently.
//!
//! # Channel is voice identity
//!
//! Under MPE a channel is not a timbre selector but a *voice slot*: the synth
//! addresses a sounding note by `NoteId::from_channel_note(channel, note)`, so
//! two same-pitch notes on different channels are independent. Dropping the
//! channel from an authored note therefore does not merely lose routing — it
//! makes overlapping same-pitch notes collide, unrecoverably.

/// One of the 16 channels within a MIDI group.
///
/// Always in `0..=15`: [`new`](Self::new) masks rather than rejects, matching
/// what the UMP constructors already do with `channel & 0x0F`.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
// `transparent` for the same reason `Samples` has it: a `MidiChannel` is `3` on
// the wire, not `[3]`.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct MidiChannel(u8);

impl MidiChannel {
    /// The first channel — and the one a single-timbral instrument uses.
    pub const FIRST: MidiChannel = MidiChannel(0);
    /// The last channel. (Channel "16" in the 1-based numbering hardware shows.)
    pub const LAST: MidiChannel = MidiChannel(15);
    /// How many channels a MIDI group carries.
    pub const COUNT: u8 = 16;

    /// Wrap a raw channel number, masking into `0..=15`.
    ///
    /// Masks rather than returning an error because that is what the wire does:
    /// the UMP field is 4 bits, so a larger value cannot be represented and
    /// `channel_voice.rs` already `debug_assert`s then masks. Rejecting here
    /// would make this type stricter than the format it addresses.
    #[inline]
    pub const fn new(raw: u8) -> MidiChannel {
        MidiChannel(raw & 0x0F)
    }

    /// The raw 0-based channel number, for a UMP constructor.
    #[inline]
    pub const fn get(self) -> u8 {
        self.0
    }

    /// The 1-based number hardware and DAW UIs display (channel 0 shows as 1).
    ///
    /// Named because the off-by-one is a display convention, and a bare `+ 1`
    /// at a call site hides that it is one.
    #[inline]
    pub const fn as_display_number(self) -> u8 {
        self.0 + 1
    }
}

impl From<MidiChannel> for u8 {
    #[inline]
    fn from(c: MidiChannel) -> u8 {
        c.0
    }
}

// Deliberately no `From<u8> for MidiChannel`: the conversion masks, and a
// lossy conversion inside a trait that promises not to lose data is exactly
// what `ChannelLayout` refused a reverse `From` for. `MidiChannel::new` names
// the masking.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_channel_is_masked_into_range() {
        assert_eq!(MidiChannel::new(0).get(), 0);
        assert_eq!(MidiChannel::new(15).get(), 15);
        // 16 wraps to 0 — the same thing the UMP constructors do, rather than a
        // silently different answer one layer up.
        assert_eq!(MidiChannel::new(16).get(), 0);
        assert_eq!(MidiChannel::new(255).get(), 15);
    }

    #[test]
    fn the_display_number_is_one_based() {
        assert_eq!(MidiChannel::FIRST.as_display_number(), 1);
        assert_eq!(MidiChannel::LAST.as_display_number(), 16);
    }

    #[test]
    fn the_default_is_the_first_channel() {
        // Load-bearing: a note authored without an explicit channel must land
        // somewhere a single-timbral instrument actually listens.
        assert_eq!(MidiChannel::default(), MidiChannel::FIRST);
    }
}
