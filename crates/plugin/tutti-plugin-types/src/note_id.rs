//! Plugin-host per-note addressing: a stable `note_id` for a `(channel, note)`
//! pair, shared by every format host that binds per-note expression to a voice.
//!
//! VST3 and CLAP both address per-note events by a host-chosen `note_id` the
//! plugin echoes back, so a note-on and any later per-note expression for the
//! *same* `(channel, note)` must compute the *same* id to bind to the same voice.
//! [`note_id_for`] is that derivation, kept in one place so the two hosts can't
//! drift apart.
//!
//! This is a **plugin-host convention**, deliberately distinct from
//! [`tutti_midi_types::NoteId`] (the engine's per-note identity): the packing
//! here is the dense `channel * 128 + note` (range `0..=2047`) that CLAP's
//! round-trip guard [`note_id_to_channel_note`] depends on, *not* `NoteId`'s
//! `(channel << 8) | note`. They must not be conflated.

/// Deterministic plugin `note_id` for a `(channel, note)` pair.
///
/// A note-on and any per-note expression for the same `(channel, note)` compute
/// the same id and therefore bind to the same voice. The mapping is a bijection
/// into `0..2048`, comfortably inside `i32`.
///
/// This is distinct from the spec's "use `noteId = -1` for channel/pitch
/// matching" fallback: that fallback only covers note-on/off, *not* note
/// expression — note-expression events carry no channel/pitch, only a `noteId`,
/// so they have no voice to attach to unless the host assigns real ids.
#[inline]
pub fn note_id_for(channel: u8, note: u8) -> i32 {
    (channel as i32) * 128 + (note as i32)
}

/// Largest `note_id` [`note_id_for`] can mint: channel 15, note 127.
pub const MAX_HOST_NOTE_ID: i32 = 15 * 128 + 127;

/// Recover the `(channel, note)` a [`note_id_for`] id was built from, or `None`
/// if `note_id` is outside the host-minted range `0..=MAX_HOST_NOTE_ID`.
///
/// A plugin may assign per-note events its *own* `note_id` space; such an id is
/// not `channel * 128 + note` and must not be force-decoded — masking it would
/// bind the event to a phantom `(channel, note)`. Callers skip the event
/// instead. (The `-1` wildcard is likewise out of range → `None`.)
#[inline]
pub fn note_id_to_channel_note(note_id: i32) -> Option<(u8, u8)> {
    if !(0..=MAX_HOST_NOTE_ID).contains(&note_id) {
        return None;
    }
    Some(((note_id / 128) as u8, (note_id % 128) as u8))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole host range is a bijection: every `(channel, note)` mints a
    /// distinct id and every id decodes back to the pair that made it. An
    /// exhaustive sweep is cheap here (2048 pairs) and is what makes the
    /// distinctness and the endpoint claims below unnecessary to state
    /// separately.
    #[test]
    fn round_trips_within_host_range() {
        for channel in 0..16u8 {
            for note in 0..128u8 {
                let id = note_id_for(channel, note);
                assert_eq!(note_id_to_channel_note(id), Some((channel, note)));
            }
        }
        // The top of the range is the constant the out-of-range guard is
        // written against, so pin the two together.
        assert_eq!(note_id_for(15, 127), MAX_HOST_NOTE_ID);
    }

    #[test]
    fn out_of_range_and_wildcard_decode_to_none() {
        assert_eq!(note_id_to_channel_note(-1), None); // wildcard
        assert_eq!(note_id_to_channel_note(MAX_HOST_NOTE_ID + 1), None);
    }
}
