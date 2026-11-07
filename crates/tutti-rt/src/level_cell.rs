//! [`LevelWriter`] / [`LevelReader`]: a level the audio thread folds each block
//! into and one reader takes whole.
//!
//! A level meter reduces every block to a peak and an energy (a sum of squares
//! over some frames). The reader wants what accumulated *since it last read*,
//! with the peak and the energy of the **same** blocks: a take that returned a
//! peak without the frames that produced it would show a spike over zero
//! frames. Separate atomic words cannot give that (a block lands between two
//! swaps), so the cell is two slots and a flip.
//!
//! # The protocol
//!
//! There are two slots, each a peak, a frame count and a sum of squares, and
//! an `active` index. Only the writer writes the active slot; only the reader
//! flips `active` and clears a slot it has flipped away from.
//!
//! - **Fold** (the audio thread, wait-free): load `active`, announce it in
//!   `busy`, `SeqCst` fence, re-check `active`; if it moved, announce again.
//!   Then fold the block into that slot (plain loads and stores of its
//!   words, the writer being alone on it) and clear `busy` with `Release`.
//! - **Take** (any one control thread): store the other index to `active`,
//!   `SeqCst` fence, wait until `busy` no longer names the slot just left
//!   (a writer inside a block, a few microseconds), then read the slot with
//!   `Acquire` and clear it.
//!
//! `busy` and `active` are the store-then-load pair of a Dekker handshake, so
//! both sides fence with `SeqCst`: either the writer sees the flip and folds
//! into the new slot, or the taker sees the announcement and waits for the
//! fold to finish. **A block lands whole in one take**, peak, frames and
//! energy together, so a take with no frames has no peak and no energy.
//! Nothing is lost or counted twice.
//!
//! The writer never waits. The reader waits only for a writer that is inside a
//! block, so it must be a control thread. A reader that never takes grows
//! nothing: a peak is a maximum, and the frame count saturates.
//!
//! The energy is an `f64`, so a long unread span (an hour of offline render
//! taken once) stays exact where an `f32` sum would stall past about 2^24
//! units.
//!
//! The model in `tests/level_cell_loom.rs` runs this code.

use sync::{Arc, AtomicU32, AtomicU64, AtomicUsize, Ordering, fence};

mod sync {
    #[cfg(loom)]
    pub(super) use loom::sync::Arc;
    #[cfg(loom)]
    pub(super) use loom::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering, fence};
    #[cfg(not(loom))]
    pub(super) use std::sync::Arc;
    #[cfg(not(loom))]
    pub(super) use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering, fence};
}

#[cfg(not(loom))]
use std::thread::yield_now;

#[cfg(loom)]
use loom::thread::yield_now;

/// What the writer folded in since the last take.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LevelSpan {
    /// The largest peak folded in (a non-negative figure, or 0).
    pub peak: f32,
    /// The sum of squares folded in.
    pub sum_sq: f64,
    /// The frames folded in. Zero means no block: then `peak` and `sum_sq`
    /// are zero too.
    pub frames: u64,
}

/// One slot: the words a block is folded into.
#[derive(Debug, Default)]
struct Slot {
    /// The bits of a non-negative `f32`.
    peak: AtomicU32,
    frames: AtomicU64,
    /// The bits of an `f64`.
    sum_sq: AtomicU64,
}

#[derive(Debug, Default)]
struct Shared {
    slots: [Slot; 2],
    /// The slot the writer folds into. Written by the reader only.
    active: AtomicUsize,
    /// `1 + slot` while the writer is inside one, else 0.
    busy: AtomicUsize,
}

/// The audio thread's end of a level cell. Not `Clone`: one writer.
#[derive(Debug)]
pub struct LevelWriter {
    shared: Arc<Shared>,
}

/// The consumer's end of a level cell. Not `Clone`: one reader, because a take
/// removes what it returns.
#[derive(Debug)]
pub struct LevelReader {
    shared: Arc<Shared>,
}

/// A writer and the reader of one level cell, both empty.
#[must_use]
pub fn level_cell() -> (LevelWriter, LevelReader) {
    let shared = Arc::new(Shared::default());
    (
        LevelWriter {
            shared: Arc::clone(&shared),
        },
        LevelReader { shared },
    )
}

impl LevelWriter {
    /// Folds one block in: its `peak` (non-negative; NaN is the caller's to
    /// exclude), its sum of squares and its `frames`. Wait-free, allocates
    /// nothing.
    ///
    /// Takes `&self` so a writer behind a shared reference can be driven by
    /// whichever thread runs its op; the caller guarantees **one thread at a
    /// time** (the executor runs a lane's op once a block). Two at once would
    /// lose a block, never touch memory unsafely.
    #[inline]
    pub fn fold(&self, peak: f32, sum_sq: f64, frames: u32) {
        let s = &*self.shared;
        let i = loop {
            let i = s.active.load(Ordering::Acquire);
            s.busy.store(1 + i, Ordering::Relaxed);
            fence(Ordering::SeqCst);
            if s.active.load(Ordering::Acquire) == i {
                break i;
            }
        };
        let slot = &s.slots[i];
        let held = f32::from_bits(slot.peak.load(Ordering::Relaxed));
        slot.peak.store(held.max(peak).to_bits(), Ordering::Relaxed);
        let n = slot.frames.load(Ordering::Relaxed);
        slot.frames
            .store(n.saturating_add(u64::from(frames)), Ordering::Relaxed);
        let sum = f64::from_bits(slot.sum_sq.load(Ordering::Relaxed));
        slot.sum_sq
            .store((sum + sum_sq).to_bits(), Ordering::Relaxed);
        s.busy.store(0, Ordering::Release);
    }
}

impl LevelReader {
    /// Takes what was folded in since the last take, and clears it. Waits for
    /// a writer that is mid-block (see the module docs): a control thread's.
    pub fn take(&mut self) -> LevelSpan {
        let s = &*self.shared;
        let old = s.active.load(Ordering::Relaxed);
        s.active.store(1 - old, Ordering::Release);
        fence(Ordering::SeqCst);
        while s.busy.load(Ordering::Acquire) == 1 + old {
            yield_now();
        }
        let slot = &s.slots[old];
        // Cleared here, ordered before the writer's next entry to this slot
        // by the `Release` store to `active` of the next take.
        LevelSpan {
            peak: f32::from_bits(slot.peak.swap(0, Ordering::Relaxed)),
            sum_sq: f64::from_bits(slot.sum_sq.swap(0, Ordering::Relaxed)),
            frames: slot.frames.swap(0, Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests;
