//! A plugin format's `note_id` for a `(channel, note)` voice.
//!
//! VST3 and CLAP address per-note events by a host-chosen `note_id` the plugin
//! echoes back, so a note-on and any later per-note expression for the *same*
//! `(channel, note)` must carry the *same* id. The id is
//! [`tutti_ump::NoteId`]'s (`channel * 128 + note`, `0..=2047`); the formats
//! carry it as an `i32`, where `-1` is the wildcard. [`plugin_note_id`] and
//! [`note_id_to_channel_note`] are that edge, the one pair every format host
//! uses.

use tutti_ump::{MidiChannel, NoteId, NoteNumber};

/// A [`NoteId`] as the formats' `i32` `note_id`: the id itself while it fits,
/// `-1` (the formats' "unspecified" wildcard) for a minted id above `i32::MAX`.
///
/// A host that sends a note-on and later per-note expression for the same note
/// sends both with this id. That is distinct from the spec's "use `noteId = -1`
/// for channel/pitch matching" fallback, which only covers note-on/off:
/// note-expression events carry no channel or pitch, only a `noteId`.
#[inline]
#[must_use]
pub fn plugin_note_id(id: NoteId) -> i32 {
    i32::try_from(id.raw()).unwrap_or(-1)
}

/// The `(channel, note)` a [`plugin_note_id`] of [`NoteId::from_channel_note`] names, or `None` for
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
    #[test]
    fn round_trips_within_host_range() {
        for channel in 0..16u8 {
            for note in 0..128u8 {
                let (channel, note) = (MidiChannel::of(channel), NoteNumber::of(note));
                let id = plugin_note_id(NoteId::from_channel_note(channel, note));
                assert_eq!(note_id_to_channel_note(id), Some((channel, note)));
            }
        }
        assert_eq!(
            plugin_note_id(NoteId::from_channel_note(
                MidiChannel::of(15),
                NoteNumber::of(127)
            )),
            NoteId::MAX_CHANNEL_NOTE as i32
        );
    }

    /// A minted id past `i32::MAX` has no `i32` form; it is the wildcard, never
    /// a wrapped negative id.
    #[test]
    fn an_id_past_i32_max_is_the_wildcard() {
        assert_eq!(plugin_note_id(NoteId::from_raw(i32::MAX as u32)), i32::MAX);
        assert_eq!(plugin_note_id(NoteId::from_raw(i32::MAX as u32 + 1)), -1);
        assert_eq!(plugin_note_id(NoteId::from_raw(u32::MAX)), -1);
    }

    #[test]
    fn out_of_range_and_wildcard_decode_to_none() {
        assert_eq!(note_id_to_channel_note(-1), None);
        assert_eq!(
            note_id_to_channel_note(NoteId::MAX_CHANNEL_NOTE as i32 + 1),
            None
        );
    }
}
