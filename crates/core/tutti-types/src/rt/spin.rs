//! [`SpinThenYield`]: how the audio thread waits for another thread's
//! progress inside a parallel block ([`TaskGraph`](super::TaskGraph)'s
//! caller, [`JobGate`](super::JobGate)'s close).
//!
//! It spins, and after [`CALLER_SPINS`] waits in a row in which the counter
//! it waits on has not moved, it yields its core once (`sched_yield`) and
//! starts counting again. A wait that sees progress within the budget is a
//! plain spin, as it always was. A wait on a helper running a task longer
//! than the budget, on another core, yields too; when no thread of the
//! caller's priority is waiting for its core, the yield returns at once.
//! `tutti-nodes`' `parallel_render` bench (`wide/32x8` at 4 participants)
//! makes about one such yield per block, with no difference in its time
//! beyond run-to-run noise.
//!
//! The yield is for the helper the caller is waiting on being queued behind
//! it on its own core. Under `SCHED_FIFO` nothing else gets that core while
//! the caller spins (FIFO does not time-slice, even between equal
//! priorities), so a caller that only spins waits for the kernel to migrate
//! the helper, or forever if it cannot: the probe in
//! `tests/spin_starvation.rs` measures a same-core FIFO pair aborted by its
//! 2 s watchdog in every run. `sched_yield` moves a FIFO caller behind the
//! threads of its own priority, so an equal-priority helper runs and
//! finishes its task. It does nothing for a helper of lower priority (or a
//! `SCHED_OTHER` one), which is why the pool's helpers must run at the
//! caller's priority (`tutti-core`'s `pool.rs`, "Priority").
//!
//! A yield is a system call that returns at once when no other thread of the
//! caller's priority is waiting for the core; it does not allocate or block.
//!
//! Under loom both the spin and the yield are `loom`'s, which schedule
//! another thread alike, so the models run this code unchanged; the budget
//! is 2 there so the yield branch is reached.

#[cfg(loom)]
use loom::{hint::spin_loop, thread::yield_now};
#[cfg(not(loom))]
use std::{hint::spin_loop, thread::yield_now};

/// Waits in a row without progress after which the caller yields its core.
///
/// A count, like the helpers' `HELPER_SPINS`, so the first this many waits
/// are the spin they always were. A `spin_loop` hint costs from a few ns to
/// about 40 ns depending on the core (Intel's `pause` since Skylake is the
/// slow end), so this is a few µs to about 40 µs without progress: a
/// fraction of a block. A future standalone crate could make it a per-pool
/// setting, or a time budget rather than a count.
pub(super) const CALLER_SPINS: u32 = if cfg!(loom) { 2 } else { 1024 };

/// A wait that spins, then yields after [`CALLER_SPINS`] waits without
/// progress. See the module docs (`src/rt/spin.rs`).
pub(super) struct SpinThenYield {
    /// Waits since the counter last moved (or since the last yield).
    spins: u32,
    /// The counter's value at the last wait.
    seen: u32,
}

impl SpinThenYield {
    /// A wait on a counter now at `seen`.
    pub(super) const fn new(seen: u32) -> Self {
        Self { spins: 0, seen }
    }

    /// One step of the wait, given the counter's current value `now`.
    pub(super) fn wait(&mut self, now: u32) {
        if now != self.seen {
            self.seen = now;
            self.spins = 0;
        }
        self.spins += 1;
        if self.spins < CALLER_SPINS {
            spin_loop();
        } else {
            self.spins = 0;
            #[cfg(all(test, not(loom)))]
            tests::count_yield();
            yield_now();
        }
    }
}

#[cfg(all(test, not(loom)))]
pub(super) mod tests {
    use std::cell::Cell;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    thread_local! {
        /// Where this thread's caller-side yields are counted, if a test asked.
        static YIELDS: Cell<Option<&'static AtomicU32>> = const { Cell::new(None) };
    }

    /// Count this thread's caller-side yields into `counter` (a test's own
    /// static), so another thread can see that this one yielded.
    pub(in crate::rt) fn count_yields_into(counter: &'static AtomicU32) {
        YIELDS.with(|y| y.set(Some(counter)));
    }

    pub(super) fn count_yield() {
        YIELDS.with(|y| {
            if let Some(c) = y.get() {
                c.fetch_add(1, Ordering::Release);
            }
        });
    }

    /// The first `CALLER_SPINS - 1` waits without progress spin, the next
    /// one yields, and the count restarts after it; progress restarts it too.
    ///
    /// Mutation: drop the reset on progress (`now != self.seen`) → the wait
    /// right after progress at `CALLER_SPINS - 1` yields → the first
    /// assertion after the progress step fails. Mutation: yield on every wait
    /// → the first loop's count is `CALLER_SPINS - 1`, not 0 → fails.
    #[test]
    fn spins_its_budget_then_yields_once() {
        static N: AtomicU32 = AtomicU32::new(0);
        count_yields_into(&N);
        let mut w = SpinThenYield::new(7);
        for _ in 0..CALLER_SPINS - 1 {
            w.wait(7);
        }
        assert_eq!(N.load(Ordering::Relaxed), 0, "the budget spins");
        w.wait(7);
        assert_eq!(N.load(Ordering::Relaxed), 1, "then one yield");
        for _ in 0..CALLER_SPINS - 2 {
            w.wait(7);
        }
        // Progress: the count restarts, so a full budget spins again.
        w.wait(6);
        for _ in 0..CALLER_SPINS - 2 {
            w.wait(6);
        }
        assert_eq!(N.load(Ordering::Relaxed), 1, "progress restarts the budget");
        w.wait(6);
        assert_eq!(N.load(Ordering::Relaxed), 2);
    }
}
