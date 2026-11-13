//! [`WakeFlag`]: a flag any thread raises and the audio thread takes, so a
//! parked node can be woken from outside the block.
//!
//! # The protocol
//!
//! - **Raise:** `store(true, Release)`. Wait-free; never allocates or locks.
//!   Raising a raised flag changes nothing: a wake is a level, not a count.
//! - **Take:** an `Acquire` load, and only when it reads `true` a
//!   `swap(false, AcqRel)`, so the common case (no wake) is one load and no
//!   read-modify-write.
//!
//! **A raise is never lost.** A take that reads `false` leaves the flag
//! alone, and the raise it did not see is in the flag for the next take. A
//! take that reads `true` swaps: that swap and a racing raise are both
//! operations on one location, so the raise lands either before the swap
//! (this take returns `true` for it) or after (it stays set for the next
//! take). **A raise is never invented:** `take` returns `true` only when the
//! swap read `true`, and only a raise writes `true`.
//!
//! The `Release`/`Acquire` pair orders what the raiser wrote before raising
//! (the state a woken node reads) before what the taker does after taking.
//! There is no store-then-load pair on either side, so no `SeqCst` fence is
//! involved.
//!
//! The model in `tests/wake_flag_loom.rs` runs this code.

use sync::{AtomicBool, Ordering};

mod sync {
    #[cfg(loom)]
    pub(super) use loom::sync::atomic::{AtomicBool, Ordering};
    #[cfg(not(loom))]
    pub(super) use std::sync::atomic::{AtomicBool, Ordering};
}

/// A flag raised from any thread and taken by the audio thread.
#[derive(Debug, Default)]
pub struct WakeFlag {
    raised: AtomicBool,
}

impl WakeFlag {
    /// A flag that is not raised.
    #[must_use]
    pub fn new() -> Self {
        Self {
            raised: AtomicBool::new(false),
        }
    }

    /// Raises the flag, from any thread. Never allocates, locks or waits.
    pub fn raise(&self) {
        self.raised.store(true, Ordering::Release);
    }

    /// Whether the flag was raised since the last take, clearing it. The
    /// audio thread's call: never allocates, locks or waits.
    ///
    /// A raise that races a take is seen by this take or the next.
    pub fn take(&self) -> bool {
        self.raised.load(Ordering::Acquire) && self.raised.swap(false, Ordering::AcqRel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A take returns a raise once, then reads clear**, and a flag nobody
    /// raised is never taken.
    #[test]
    fn a_raise_is_taken_once() {
        let flag = WakeFlag::new();
        assert!(!flag.take(), "nothing raised");
        flag.raise();
        flag.raise();
        assert!(flag.take(), "the raise");
        assert!(!flag.take(), "a level, taken whole");
    }
}
