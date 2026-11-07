//! [`MidiChannel`] — which of a MIDI group's 16 channels a message addresses.
//!
//! An *address*, not a measure — so like `Samples` it is an
//! integer newtype rather than one of the float-backed units in
//! `tutti_types`, and it deliberately does not implement
//! `Unit`: a channel is not an automatable DSP parameter and must
//! not be reachable through `Param`.
//!
//! # Why a newtype and not `u8`
//!
//! Every UMP constructor takes `(group, channel, note, …)` — three adjacent
//! `u8`s. Nothing stops two of them being swapped, and the result is not a
//! crash: the note plays on the wrong channel, which under MPE means the wrong
//! *voice*, and per-note expression lands on a note nobody is holding.
//!
//! A `pub type MidiChannel = u8` alias prevents none of that — an alias is the
//! same type. This is a real newtype, serializable (feature `serde`) so a document can persist a channel.
//!
//! See [`MidiGroup`](crate::MidiGroup) for the other half: typing a channel
//! alone still leaves `(group, channel)` transposable, since both are 4-bit.
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
/// Always in `0..=15`: [`new`](Self::new) refuses a larger number, and
/// [`masked`](Self::masked) is the one constructor that masks.
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

    /// The channel, or `None` when `raw` is above 15.
    ///
    /// ```
    /// use tutti_ump::MidiChannel;
    ///
    /// assert_eq!(MidiChannel::new(9).map(MidiChannel::display_number), Some(10));
    /// assert_eq!(MidiChannel::new(16), None);
    /// ```
    #[inline]
    #[must_use]
    pub const fn new(raw: u8) -> Option<MidiChannel> {
        if raw < Self::COUNT {
            Some(MidiChannel(raw))
        } else {
            None
        }
    }

    /// The channel for a literal or a constant. For a value that arrives from
    /// outside, use [`new`](Self::new).
    ///
    /// # Panics
    ///
    /// When `raw` is above 15; in a `const` that is a compile error.
    #[inline]
    #[must_use]
    #[track_caller]
    pub const fn of(raw: u8) -> MidiChannel {
        match Self::new(raw) {
            Some(channel) => channel,
            None => panic!("a channel is 0 to 15"),
        }
    }

    /// The low four bits of `raw`: the one place that masks. Use it where the
    /// bits are known to be a Channel field, as the decoder does; a value from
    /// outside the wire is checked with [`new`](Self::new).
    #[inline]
    #[must_use]
    pub const fn masked(raw: u8) -> MidiChannel {
        MidiChannel(raw & 0x0F)
    }

    /// Returns the raw 0-based channel number, for a UMP constructor.
    #[inline]
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }

    /// Returns the 1-based number hardware and DAW UIs display (channel 0
    /// shows as 1).
    ///
    /// Named because the off-by-one is a display convention, and a bare `+ 1`
    /// at a call site hides that it is one.
    #[inline]
    #[must_use]
    pub const fn display_number(self) -> u8 {
        self.0 + 1
    }
}

impl From<MidiChannel> for u8 {
    #[inline]
    fn from(c: MidiChannel) -> u8 {
        c.0
    }
}

// Deliberately no `From<u8> for MidiChannel`: the conversion would mask or
// fail, and a lossy conversion inside a trait that promises not to lose data
// is exactly what `ChannelLayout` refused a reverse `From` for.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_channel_out_of_range_is_refused() {
        for raw in 0..=255u8 {
            match MidiChannel::new(raw) {
                Some(channel) => {
                    assert!(raw < 16);
                    assert_eq!(channel.get(), raw);
                    assert_eq!(MidiChannel::of(raw), channel);
                }
                None => assert!(raw >= 16),
            }
        }
    }

    #[test]
    fn masking_is_only_through_masked() {
        assert_eq!(MidiChannel::masked(16).get(), 0);
        assert_eq!(MidiChannel::masked(255).get(), 15);
        assert_eq!(MidiChannel::masked(9), MidiChannel::of(9));
    }

    #[test]
    #[should_panic(expected = "a channel is 0 to 15")]
    fn of_panics_out_of_range() {
        let _ = MidiChannel::of(16);
    }

    #[test]
    fn the_display_number_is_one_based() {
        assert_eq!(MidiChannel::FIRST.display_number(), 1);
        assert_eq!(MidiChannel::LAST.display_number(), 16);
    }
}
