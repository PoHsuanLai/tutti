//! [`SpinThenYield`]: how the audio thread waits for another thread's
//! progress inside a parallel block ([`TaskGraph`](super::TaskGraph)'s
//! caller, [`JobGate`](super::JobGate)'s close).
//!
//! It spins. On Linux (and Android), once the counter it waits on has not
//! moved for `CALLER_YIELD_AFTER` (250 µs), it yields its core once
//! (`sched_yield`) and starts timing again. Any move of the counter restarts
//! the clock. The clock is read once every `CLOCK_EVERY` waits, never per
//! wait. A healthy block never gets there: a quarter of a millisecond with
//! no task finishing anywhere is a stuck helper or a single task of that
//! length. In `tutti-nodes`' `parallel_render` bench the caller yields
//! only in the odd block whose helper was off its core (doc 013, "The
//! caller yields to a same-core helper").
//!
//! # Why yield
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
//! # What a yield costs
//!
//! It is one system call; it does not allocate. Under `SCHED_FIFO` or
//! `SCHED_RR` it returns at once when no other thread of the caller's
//! priority is ready on its core. Under `SCHED_OTHER` (a host that does not
//! promote its callback, an offline export) the fair scheduler may run
//! another task first, for up to a few milliseconds. That is why the budget
//! is long enough that only a stuck block yields.
//!
//! # Only on Linux
//!
//! Elsewhere the caller only spins, as it always did. Windows'
//! `SwitchToThread` may run a ready thread of *lower* priority for up to its
//! quantum, and macOS's `swtch_pri` lowers the caller's priority for up to
//! a quantum: either can cost a heavy but healthy block its deadline. Both
//! schedulers also round-robin equal-priority real-time threads at a
//! quantum's end, so a same-core helper is not starved for good there.
//! Neither platform was measured.
//!
//! # Under loom
//!
//! loom has no clock, so the budget is a count there: every second wait
//! without progress yields. Both the spin and the yield are `loom`'s, which
//! schedule another thread alike, so the models run this code, the progress
//! tracking included.

#[cfg(not(loom))]
use std::hint::spin_loop;
#[cfg(all(not(loom), any(target_os = "linux", target_os = "android")))]
use std::thread::yield_now;
#[cfg(all(not(loom), any(target_os = "linux", target_os = "android")))]
use std::time::{Duration, Instant};

#[cfg(loom)]
use loom::{hint::spin_loop, thread::yield_now};

/// How long the counter may stay unmoved before the caller yields its core.
///
/// Long enough that a healthy block never reaches it: no task finishing
/// anywhere for 250 µs means a stuck helper, or one task that long. Short
/// enough to break a same-core starvation with the block still alive: at
/// 128 frames and 48 kHz (2.67 ms) the helper gets the core back with about
/// 2.4 ms to spare, and at 64 frames (1.33 ms), with about 1 ms. A future
/// standalone crate could make it a per-pool setting.
#[cfg(all(not(loom), any(target_os = "linux", target_os = "android")))]
pub(super) const CALLER_YIELD_AFTER: Duration = Duration::from_micros(250);

/// Waits between two reads of the clock. A wait is a `spin_loop` hint plus
/// the caller's own polling (a ready-list `pop` attempt and a load for the
/// task graph, a load for the gate). That takes from about 10 ns to about
/// 100 ns depending on the core and on contention: a `pause` is about 14 ns
/// on the VM measured in doc 013 and 50 to 70 ns on Intel since Skylake.
/// So the clock is read every 1 to 6 µs, which is fine-grained next to the
/// budget. A read (`clock_gettime` through the vDSO) is about 20 ns, so
/// this costs well under 1% of a spin.
#[cfg(all(not(loom), any(target_os = "linux", target_os = "android")))]
pub(super) const CLOCK_EVERY: u32 = 64;

/// Under loom: waits without progress per yield.
#[cfg(loom)]
const LOOM_SPINS: u32 = 2;

/// A wait that spins, then yields its core after `CALLER_YIELD_AFTER`
/// without progress (on Linux). See the module docs (`src/rt/spin.rs`).
pub(super) struct SpinThenYield {
    /// The counter's value at the last wait.
    seen: u32,
    /// Waits since the last clock read (or, under loom, since progress or
    /// the last yield).
    #[cfg(any(loom, target_os = "linux", target_os = "android"))]
    spins: u32,
    /// When the counter was first seen unmoved, by the clock's reads; `None`
    /// until the first read after progress or a yield.
    #[cfg(all(not(loom), any(target_os = "linux", target_os = "android")))]
    since: Option<Instant>,
}

impl SpinThenYield {
    /// A wait on a counter now at `seen`.
    pub(super) const fn new(seen: u32) -> Self {
        Self {
            seen,
            #[cfg(any(loom, target_os = "linux", target_os = "android"))]
            spins: 0,
            #[cfg(all(not(loom), any(target_os = "linux", target_os = "android")))]
            since: None,
        }
    }

    /// One step of the wait, given the counter's current value `now`.
    pub(super) fn wait(&mut self, now: u32) {
        if now != self.seen {
            self.seen = now;
            self.restart();
        }
        if self.due() {
            self.restart();
            #[cfg(all(test, not(loom)))]
            tests::count_yield();
            #[cfg(any(loom, target_os = "linux", target_os = "android"))]
            yield_now();
        } else {
            spin_loop();
        }
    }

    /// Start the budget again: after progress, or after a yield.
    fn restart(&mut self) {
        #[cfg(any(loom, target_os = "linux", target_os = "android"))]
        {
            self.spins = 0;
        }
        #[cfg(all(not(loom), any(target_os = "linux", target_os = "android")))]
        {
            self.since = None;
        }
    }

    /// Whether this wait spends the budget.
    #[cfg(loom)]
    fn due(&mut self) -> bool {
        self.spins += 1;
        self.spins >= LOOM_SPINS
    }

    /// Whether this wait spends the budget: every `CLOCK_EVERY` waits, a
    /// clock read, and the first read after a restart starts the clock.
    #[cfg(all(not(loom), any(target_os = "linux", target_os = "android")))]
    fn due(&mut self) -> bool {
        self.spins += 1;
        if self.spins < CLOCK_EVERY {
            return false;
        }
        self.spins = 0;
        let t = now();
        match self.since {
            None => {
                self.since = Some(t);
                false
            }
            Some(s) => t.saturating_duration_since(s) >= CALLER_YIELD_AFTER,
        }
    }

    /// Off Linux the caller only spins (see the module docs, "Only on
    /// Linux").
    #[cfg(all(not(loom), not(any(target_os = "linux", target_os = "android"))))]
    fn due(&mut self) -> bool {
        false
    }
}

/// The clock: `Instant::now`, or a test's stepped clock on its thread.
#[cfg(all(not(loom), any(target_os = "linux", target_os = "android")))]
fn now() -> Instant {
    #[cfg(test)]
    if let Some(t) = tests::stepped_now() {
        return t;
    }
    Instant::now()
}

#[cfg(all(test, not(loom)))]
pub(super) mod tests {
    use std::cell::Cell;
    use std::sync::atomic::{AtomicU32, Ordering};

    thread_local! {
        /// Where this thread's caller-side yields are counted, if a test asked.
        static YIELDS: Cell<Option<&'static AtomicU32>> = const { Cell::new(None) };
        /// A stepped clock for this thread, if a test asked: the next read
        /// and the step each read advances it by.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        static CLOCK: Cell<Option<(std::time::Instant, std::time::Duration)>> =
            const { Cell::new(None) };
    }

    /// Count this thread's caller-side yields into `counter` (a test's own
    /// static), so another thread can see that this one yielded.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub(in crate::rt) fn count_yields_into(counter: &'static AtomicU32) {
        YIELDS.with(|y| y.set(Some(counter)));
    }

    /// Replace this thread's clock with one that advances `step` per read,
    /// so a wait's yield depends on how many waits it made and not on how
    /// long they took.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub(in crate::rt) fn step_clock(step: std::time::Duration) {
        CLOCK.with(|c| c.set(Some((std::time::Instant::now(), step))));
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub(super) fn stepped_now() -> Option<std::time::Instant> {
        CLOCK.with(|c| {
            let (t, step) = c.get()?;
            c.set(Some((t + step, step)));
            Some(t)
        })
    }

    pub(super) fn count_yield() {
        YIELDS.with(|y| {
            if let Some(c) = y.get() {
                c.fetch_add(1, Ordering::Release);
            }
        });
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    mod linux {
        use std::sync::atomic::{AtomicU32, Ordering};

        use super::super::*;
        use super::*;

        /// With the clock stepping a quarter of the budget per read, the
        /// first read starts the clock and the fifth reaches the budget: the
        /// wait yields on its `5 * CLOCK_EVERY`th wait without progress, and
        /// not before. Progress restarts the clock.
        ///
        /// Mutation: drop the restart on progress (`now != self.seen`) → the
        /// clock started before the progress keeps running, and the second
        /// segment yields early → the "progress restarts" assertion fails.
        /// Mutation: yield on every clock read past the first (ignore the
        /// elapsed time) → the first assertion counts a yield → fails.
        #[test]
        fn yields_once_the_clock_passes_the_budget() {
            static N: AtomicU32 = AtomicU32::new(0);
            count_yields_into(&N);
            step_clock(CALLER_YIELD_AFTER / 4);
            let at = 5 * CLOCK_EVERY;
            let mut w = SpinThenYield::new(7);
            for _ in 0..at - 1 {
                w.wait(7);
            }
            assert_eq!(N.load(Ordering::Relaxed), 0, "inside the budget it spins");
            w.wait(7);
            assert_eq!(N.load(Ordering::Relaxed), 1, "then one yield");
            for _ in 0..at - 1 {
                w.wait(7);
            }
            // Progress: the clock restarts, so a full budget spins again.
            w.wait(6);
            for _ in 0..at - 2 {
                w.wait(6);
            }
            assert_eq!(N.load(Ordering::Relaxed), 1, "progress restarts the clock");
            w.wait(6);
            assert_eq!(N.load(Ordering::Relaxed), 2);
        }

        /// A wait whose clock never passes the budget never yields, however
        /// many waits it makes: the budget is time, not a count.
        ///
        /// Mutation: yield after a fixed count of clock reads, whatever they
        /// say → fails.
        #[test]
        fn a_wait_inside_the_budget_never_yields() {
            static N: AtomicU32 = AtomicU32::new(0);
            count_yields_into(&N);
            step_clock(std::time::Duration::ZERO);
            let mut w = SpinThenYield::new(7);
            for _ in 0..100 * CLOCK_EVERY {
                w.wait(7);
            }
            assert_eq!(N.load(Ordering::Relaxed), 0);
        }
    }
}
