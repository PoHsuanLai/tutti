//! Putting a block's MIDI in the order an event port takes it.

use tutti_graph::Offset;
use tutti_graph::UmpAt;
use tutti_types::Samples;
use tutti_ump::Ump;

/// An empty slot for a buffer of events: a Utility NOOP at the block's first
/// frame.
pub(crate) const BLANK: UmpAt = (Offset::ZERO, Ump([0; 4]));

/// Sort by offset (or raw stamp), keeping arrival order among equal offsets. (An
/// offset past the block is clamped onto its last frame by [`offset_of`],
/// which keeps this order.)
///
/// An insertion sort: it never allocates (a stable `sort_by_key` would), and
/// what arrives from a wire or a ring is already in order or nearly so.
pub(crate) fn in_offset_order<T: Ord + Copy>(events: &mut [(T, Ump)]) {
    for i in 1..events.len() {
        let mut j = i;
        while j > 0 && events[j - 1].0 > events[j].0 {
            events.swap(j - 1, j);
            j -= 1;
        }
    }
}

/// `e`'s offset in a block of `block_len` frames, clamped into it.
pub(crate) fn offset_of(e: &UmpAt, block_len: Samples) -> Offset {
    Offset::clamped(e.0.get(), block_len)
}

#[cfg(test)]
mod tests;
