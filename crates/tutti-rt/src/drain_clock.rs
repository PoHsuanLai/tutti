//! [`drain_clock()`]: how many times the audio side has begun to drain what the
//! control side hands it, read by the control side to tell which of its writes
//! a block has surely taken.
//!
//! The control thread sends a write to the audio thread through a queue, and
//! sometimes needs to know whether a block has taken it yet (a save must add
//! the writes the plugin has not seen). The audio thread counts each time it
//! begins to drain; the control thread reads the count after sending, as a
//! [`DrainMark`], and asks later whether two drains have begun since.
//!
//! # The protocol
//!
//! - **Begin** ([`DrainBeat::begin`]): `fetch_add(1, Release)`, then a `SeqCst`
//!   fence. The audio thread drains after it.
//! - **Mark** ([`DrainWatch::mark`]): a `SeqCst` fence, then `load(Acquire)`.
//!   The control thread sends, then marks.
//! - **Ask** ([`DrainWatch::two_since`]): `load(Acquire)`, true when the count
//!   is at least two past the mark.
//!
//! The fences are a store-load (Dekker) pair: the send is a store the audio
//! thread's drain loads, and the count is a store the control thread's mark
//! loads. The two fences are totally ordered, so either the drain began after
//! the send and sees the write, or the mark read a count that already holds
//! that drain's begin. A drain counted in the mark may have read the queue
//! before the write; the next one began after the mark was read, so it began
//! after the send and sees the write. That is why two drains, not one: the
//! drain after the mark is the first that must have seen the write, and the
//! audio thread runs one block at a time, so the one after it began only when
//! that block was over. Fences rather than `SeqCst` accesses because a fence is
//! what `loom` models exactly, as in `job_gate.rs`.
//!
//! Each side holds only what it may do: the audio side can begin a drain and
//! nothing else, the control side can read.
//!
//! The model in `tests/drain_clock_loom.rs` runs this code.

use std::sync::Arc;

use sync::{AtomicU64, Ordering, fence};

mod sync {
    #[cfg(loom)]
    pub(super) use loom::sync::atomic::{AtomicU64, Ordering, fence};
    #[cfg(not(loom))]
    pub(super) use std::sync::atomic::{AtomicU64, Ordering, fence};
}

/// The audio side's half: counts drains. One per clock, not `Clone`.
#[derive(Debug)]
pub struct DrainBeat {
    drains: Arc<AtomicU64>,
}

/// The control side's half: marks and asks.
#[derive(Debug, Clone)]
pub struct DrainWatch {
    drains: Arc<AtomicU64>,
}

/// The drain count read after a write was sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrainMark(u64);

/// A drain clock: the audio side's half and the control side's.
#[must_use]
pub fn drain_clock() -> (DrainBeat, DrainWatch) {
    let drains = Arc::new(AtomicU64::new(0));
    (
        DrainBeat {
            drains: drains.clone(),
        },
        DrainWatch { drains },
    )
}

impl DrainBeat {
    /// Counts a drain begun. Call it once per block, before draining the
    /// queue. Wait-free, allocation-free.
    pub fn begin(&mut self) {
        self.drains.fetch_add(1, Ordering::Release);
        fence(Ordering::SeqCst);
    }
}

impl DrainWatch {
    /// The count, read after sending a write: call it after the send, not
    /// before.
    pub fn mark(&self) -> DrainMark {
        fence(Ordering::SeqCst);
        DrainMark(self.drains.load(Ordering::Acquire))
    }

    /// Whether two drains have begun since `mark`: the write sent before it
    /// has surely been drained by a block that has ended.
    #[must_use]
    pub fn two_since(&self, mark: DrainMark) -> bool {
        self.drains.load(Ordering::Acquire) >= mark.0 + 2
    }
}
