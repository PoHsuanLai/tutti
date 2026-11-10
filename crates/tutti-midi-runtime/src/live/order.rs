//! Putting a block's MIDI in the order an event port takes it.

use tutti_graph::Offset;
use tutti_types::Samples;
use tutti_ump::Ump;

/// An empty slot for a buffer of events: a Utility NOOP at the block's first
/// frame.
pub(crate) const BLANK: (Offset, Ump) = (Offset::ZERO, Ump([0; 4]));

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
pub(crate) fn offset_of(e: &(Offset, Ump), block_len: Samples) -> Offset {
    clamp_stamp(e.0.get(), block_len)
}

/// A raw frame stamp (a mailbox's) as an offset in a block of `block_len`
/// frames, clamped onto its last frame.
pub(crate) fn clamp_stamp(stamp: u32, block_len: Samples) -> Offset {
    let last = block_len.get().saturating_sub(1);
    Offset::new((stamp as usize).min(last), block_len).unwrap_or(Offset::ZERO)
}

#[cfg(test)]
mod tests;
