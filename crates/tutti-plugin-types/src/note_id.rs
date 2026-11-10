//! A plugin format's `note_id` for a `(channel, note)` voice.
//!
//! VST3 and CLAP address per-note events by a host-chosen `note_id` the plugin
//! echoes back, so a note-on and any later per-note expression for the *same*
//! `(channel, note)` must carry the *same* id. The id is
//! [`tutti_ump::NoteId`]'s (`channel * 128 + note`, `0..=2047`); the formats
//! carry it as an `i32`, where `-1` is the wildcard. These two functions are
//! that edge.

use tutti_ump::{MidiChannel, NoteId, NoteNumber};

/// The plugin `note_id` of `(channel, note)`: [`NoteId::from_channel_note`]
/// as the formats' `i32`.
///
/// This is distinct from the spec's "use `noteId = -1` for channel/pitch
/// matching" fallback: that fallback only covers note-on/off, *not* note
/// expression. Note-expression events carry no channel/pitch, only a `noteId`,
/// so they have no voice to attach to unless the host assigns real ids.
#[inline]
#[must_use]
pub fn note_id_for(channel: MidiChannel, note: NoteNumber) -> i32 {
    // At most `NoteId::MAX_CHANNEL_NOTE` (2047): it fits.
    NoteId::from_channel_note(channel, note).raw() as i32
}

/// The `(channel, note)` a [`note_id_for`] id was built from, or `None` for
/// any other id.
///
/// A plugin may assign per-note events its *own* `note_id` space; such an id is
/// not `channel * 128 + note` and must not be force-decoded: masking it would
/// bind the event to a phantom `(channel, note)`. Callers skip the event
/// instead. (The `-1` wildcard is likewise `None`.)
#[inline]
#[must_use]
pub fn note_id_to_channel_note(note_id: i32) -> Option<(MidiChannel, NoteNumber)> {
    NoteId::from_raw(u32::try_from(note_id).ok()?).channel_note()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole host range is a bijection: every `(channel, note)` mints a
    /// distinct id and every id decodes back to the pair that made it.
    ///
    /// Mutation (run): `note_id_for` returning `note * 16 + channel` → the
    /// pairs no longer decode → fails.
    #[test]
    fn round_trips_within_host_range() {
        for channel in 0..16u8 {
            for note in 0..128u8 {
                let (channel, note) = (MidiChannel::of(channel), NoteNumber::of(note));
                let id = note_id_for(channel, note);
                assert_eq!(note_id_to_channel_note(id), Some((channel, note)));
            }
        }
        assert_eq!(
            note_id_for(MidiChannel::of(15), NoteNumber::of(127)),
            NoteId::MAX_CHANNEL_NOTE as i32
        );
    }

    /// Mutation (run): `note_id_to_channel_note` decoding `id / 128` and
    /// `id % 128` without the range check → the id past the range decodes →
    /// fails.
    #[test]
    fn out_of_range_and_wildcard_decode_to_none() {
        assert_eq!(note_id_to_channel_note(-1), None);
        assert_eq!(
            note_id_to_channel_note(NoteId::MAX_CHANNEL_NOTE as i32 + 1),
            None
        );
    }
}
