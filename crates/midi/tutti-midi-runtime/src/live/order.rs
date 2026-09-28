//! Putting a block's MIDI in the order an event port takes it.

use tutti_core::Samples;
use tutti_graph::Offset;
use tutti_midi_types::ump::MidiEvent;

/// Sort by `frame_offset`, keeping arrival order among equal offsets. (An
/// offset past the block is clamped onto its last frame by [`offset_of`],
/// which keeps this order.)
///
/// An insertion sort: it never allocates (a stable `sort_by_key` would), and
/// what arrives from a wire or a ring is already in order or nearly so.
pub(crate) fn in_offset_order(events: &mut [MidiEvent]) {
    for i in 1..events.len() {
        let mut j = i;
        while j > 0 && events[j - 1].frame_offset > events[j].frame_offset {
            events.swap(j - 1, j);
            j -= 1;
        }
    }
}

/// `e`'s offset in a block of `block_len` frames, clamped into it.
pub(crate) fn offset_of(e: &MidiEvent, block_len: Samples) -> Offset {
    let last = block_len.get().saturating_sub(1);
    Offset::new((e.frame_offset as usize).min(last), block_len).unwrap_or(Offset::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_midi_types::{MidiChannel, MidiGroup};

    fn at(note: u8, offset: u32) -> MidiEvent {
        MidiEvent::note_on_7bit(MidiGroup::FIRST, MidiChannel::FIRST, note, 100)
            .with_frame_offset(offset)
    }

    /// Sorted by offset, arrival order kept at one offset; a late offset
    /// placed on the block's last frame.
    ///
    /// Mutation (run): `>=` for `>` in the sort (unstable at ties) → 2
    /// before 1 → fails. Mutation (run): no clamp in `offset_of` → `None`,
    /// placed on frame 0 → fails.
    #[test]
    fn sorts_stably_and_places_late_offsets_last() {
        let mut v = [at(1, 10), at(0, 5), at(2, 10), at(4, 900), at(3, 0)];
        in_offset_order(&mut v);
        let got: Vec<(u8, u32)> = v
            .iter()
            .map(|e| (e.note().unwrap(), e.frame_offset))
            .collect();
        assert_eq!(got, vec![(3, 0), (0, 5), (1, 10), (2, 10), (4, 900)]);
        assert_eq!(offset_of(&v[4], Samples(64)).get(), 63);
    }
}
