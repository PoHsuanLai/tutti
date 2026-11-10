//! Per-note identity for MIDI 2.0 per-note addressing.

use crate::{MidiChannel, NoteNumber};

/// A note's identity inside the host: what keeps two simultaneous notes of the
/// same number apart.
///
/// MIDI 2.0 has no note-id field on the wire: a per-note message names a
/// channel and a note number (M2-104). A note named that way is
/// [`from_channel_note`](Self::from_channel_note), `channel * 128 + note`, so
/// its id is a pure function of the pair and [`channel_note`](Self::channel_note)
/// recovers it. That is also the id the plugin formats carry on their wire (a
/// CLAP or VST3 `note_id` is this value as an `i32`): there is one formula.
///
/// An allocator that gives two notes of one number their own identity mints
/// ids above [`MAX_CHANNEL_NOTE`](Self::MAX_CHANNEL_NOTE) with
/// [`from_raw`](Self::from_raw); those name no pair.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct NoteId(u32);

impl NoteId {
    /// The largest id a `(channel, note)` pair names: channel 16, note 127.
    pub const MAX_CHANNEL_NOTE: u32 = 15 * 128 + 127;

    /// The id of `note` on `channel`: `channel * 128 + note`.
    #[inline]
    #[must_use]
    pub const fn from_channel_note(channel: MidiChannel, note: NoteNumber) -> Self {
        Self(channel.get() as u32 * 128 + note.get() as u32)
    }

    /// An id as it was stored or minted. One above
    /// [`MAX_CHANNEL_NOTE`](Self::MAX_CHANNEL_NOTE) names no pair.
    #[inline]
    #[must_use]
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// The pair this id names, or `None` for a minted id (above
    /// [`MAX_CHANNEL_NOTE`](Self::MAX_CHANNEL_NOTE)).
    #[inline]
    #[must_use]
    pub const fn channel_note(self) -> Option<(MidiChannel, NoteNumber)> {
        if self.0 > Self::MAX_CHANNEL_NOTE {
            return None;
        }
        Some((
            MidiChannel::masked((self.0 / 128) as u8),
            NoteNumber::masked((self.0 % 128) as u8),
        ))
    }

    /// The stored value.
    #[inline]
    #[must_use]
    pub const fn raw(self) -> u32 {
        self.0
    }
}

impl From<NoteId> for u32 {
    fn from(id: NoteId) -> Self {
        id.0
    }
}
