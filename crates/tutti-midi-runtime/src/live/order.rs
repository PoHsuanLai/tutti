//! Putting a block's MIDI in the order an event port takes it.

use tutti_graph::Offset;
use tutti_midi_types::ump::MidiEvent;
use tutti_types::Samples;

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
mod tests;
