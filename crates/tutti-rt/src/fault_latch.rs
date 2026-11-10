//! [`FaultLatch`]: a flag a producer raises when it stops for good, read by a
//! consumer that must see every item pushed before the raise before it
//! believes the stop.
//!
//! A live source (a capture callback) pushes frames into a ring and, when its
//! device goes away, raises a flag. The consumer pulls from the ring and asks
//! whether the source has failed. Asked in the wrong order the answer is wrong
//! silently: a consumer that finds the ring empty and *then* reads the flag can
//! see a flag raised after frames that landed between the two reads, and
//! report "failed, nothing left" with frames still in the ring: the take loses
//! its last block with no error. Asked in the right order the flag read can
//! only be stale in the safe direction.
//!
//! # The protocol
//!
//! - **Raise** ([`FaultLatch::raise`]): a `Release` store. The producer pushes
//!   its items first, then raises.
//! - **Drain** ([`FaultLatch::drain`]): an `Acquire` load of the flag **first**,
//!   then the pop. The verdict is [`Drained::Failed`] only if the flag was
//!   raised *and* the pop found nothing. The pop that follows a raised flag
//!   sees every item pushed before the raise (the `Acquire` pairs with the
//!   `Release`), so an empty pop after a raised flag is a drained ring. A flag
//!   raised after the load is read on the next call; the pop may then have
//!   found an item or not, and either is right.
//!
//! There is no fence: it is one load and one store on one flag, a
//! release/acquire pair, not a store-load pair.
//!
//! The model in `tests/fault_latch_loom.rs` runs this code, with a loom ring
//! standing in for the one the items travel through.

use sync::{AtomicBool, Ordering};

mod sync {
    #[cfg(loom)]
    pub(super) use loom::sync::atomic::{AtomicBool, Ordering};
    #[cfg(not(loom))]
    pub(super) use std::sync::atomic::{AtomicBool, Ordering};
}

/// What one [`FaultLatch::drain`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drained<T> {
    /// The pop produced an item.
    Item(T),
    /// The pop found nothing and the latch was not raised: the producer is
    /// late, not gone.
    Empty,
    /// The latch was raised before the pop and the pop found nothing: the
    /// producer is gone and everything it pushed has been taken.
    Failed,
}

/// A flag that a producer raises when it stops for good.
#[derive(Debug, Default)]
pub struct FaultLatch {
    raised: AtomicBool,
}

impl FaultLatch {
    /// A latch that is not raised.
    #[must_use]
    pub fn new() -> Self {
        Self {
            raised: AtomicBool::new(false),
        }
    }

    /// Raises the latch: the producer will push nothing more. Push every item
    /// first; what is pushed after the raise may be missed by a drain that saw
    /// it. Never waits.
    pub fn raise(&self) {
        self.raised.store(true, Ordering::Release);
    }

    /// Lowers the latch, for a producer that starts again.
    pub fn clear(&self) {
        self.raised.store(false, Ordering::Release);
    }

    /// Whether the latch is raised. For a reader with no ring to drain; a
    /// consumer that pulls items uses [`drain`](Self::drain), which orders the
    /// read before its pop.
    #[must_use]
    pub fn is_raised(&self) -> bool {
        self.raised.load(Ordering::Acquire)
    }

    /// Reads the latch, **then** runs `pop`, and says what the pair means.
    ///
    /// See the module docs above for why the order is the whole point.
    pub fn drain<T>(&self, pop: impl FnOnce() -> Option<T>) -> Drained<T> {
        let raised = self.is_raised();
        match pop() {
            Some(item) => Drained::Item(item),
            None if raised => Drained::Failed,
            None => Drained::Empty,
        }
    }
}
