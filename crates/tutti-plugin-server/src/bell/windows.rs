//! The bell where the main loop's wait has no descriptor to watch (Windows,
//! and the fallback platforms): a flag. The loop's wait naps in slices of at
//! most 1 ms and checks [`Bell::rung`] between them, so a ring ends the wait
//! within a millisecond.

use std::sync::atomic::{AtomicBool, Ordering};

/// The process's bell. See the module docs.
pub(crate) struct Bell {
    rung: AtomicBool,
}

impl Bell {
    pub(super) fn new() -> Self {
        Self {
            rung: AtomicBool::new(false),
        }
    }

    /// Makes the main loop's wait end. Any thread, the audio thread included.
    pub(crate) fn ring(&self) {
        self.rung.store(true, Ordering::Release);
    }

    /// Whether it rang since the last call, clearing it. The main loop's.
    pub(crate) fn take(&self) -> bool {
        self.rung.swap(false, Ordering::AcqRel)
    }

    /// Whether the bell has rung since the last [`take`](Self::take): what
    /// the wait checks between naps.
    pub(crate) fn rung(&self) -> bool {
        self.rung.load(Ordering::Acquire)
    }
}
