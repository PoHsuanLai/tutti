//! How a participant waits inside a parallel block, and for how long:
//! [`SpinThenYield`] for the audio thread ([`TaskGraph`](super::TaskGraph)'s
//! caller, [`JobGate`](super::JobGate)'s close), [`SpinThenLeave`] for a
//! pool's helper, and [`BlockWaits`], the two budgets, set per pool.
//!
//! The caller spins. On Linux (and Android), once the counter it waits on
//! has not moved for [`BlockWaits::caller_yield_after`] (250 µs by
//! default), it yields its core once (`sched_yield`) and starts timing
//! again. Any move of the counter restarts the clock. The clock is read once
//! every `CLOCK_EVERY` waits, never per wait. A healthy block never gets
//! there: a quarter of a millisecond with no task finishing anywhere is a
//! stuck helper or a single task of that length. In `tutti-nodes`'
//! `parallel_render` bench the caller yields only in the odd block whose
//! helper was off its core (doc 013, "The caller yields to a same-core
//! helper").
//!
//! A helper with nothing to take spins for
//! [`BlockWaits::helper_leave_after`] (15 µs by default), on the same clock
//! read as rarely, and then leaves the block (`task_graph.rs`, "Waiting").
//! That holds on every platform: leaving involves no scheduler.
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
//! loom has no clock, so both budgets are counts there, whatever the
//! [`BlockWaits`]: the caller yields on every second wait without progress,
//! and a helper leaves on its third wait with nothing to take. Both the spin
//! and the yield are `loom`'s, which schedule another thread alike, so the
//! models run this code, the progress tracking included.

use std::time::Duration;

#[cfg(not(loom))]
use std::hint::spin_loop;
#[cfg(all(not(loom), any(target_os = "linux", target_os = "android")))]
use std::thread::yield_now;
#[cfg(not(loom))]
use std::time::Instant;

#[cfg(loom)]
use loom::{hint::spin_loop, thread::yield_now};

/// How long a participant of a parallel block waits before it gives way:
/// the audio thread by yielding its core once, a helper by leaving the
/// block. A pool sets them (`tutti_core::WorkerPoolBuilder`), and the
/// executor hands its pool's to [`TaskGraph::work`](super::TaskGraph::work);
/// a [`JobGate`](super::JobGate) holds its own for its close. See the
/// module docs (`src/rt/spin.rs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockWaits {
    /// How long the counter the audio thread waits on may stay unmoved
    /// before it yields its core once (Linux and Android; elsewhere it only
    /// spins). Default 250 µs: long enough that a healthy block never
    /// reaches it, since no task finishing anywhere for 250 µs means a stuck
    /// helper, or one task that long; short enough to break a same-core
    /// starvation with the block still alive: at 128 frames and 48 kHz
    /// (2.67 ms) the helper gets the core back with about 2.4 ms to spare,
    /// and at 64 frames (1.33 ms), with about 1 ms. Zero yields after
    /// `2 * CLOCK_EVERY` (128) waits without progress: the clock is read
    /// every `CLOCK_EVERY` waits, and its first read only starts it.
    pub caller_yield_after: Duration,
    /// How long a helper with nothing to take spins before it leaves the
    /// block. Long enough to pick up the successors of a task another
    /// participant is finishing, short enough that a helper behind a long
    /// critical-path task goes back to its pool. Default 15 µs, about 1024
    /// waits where it was measured: a helper's 1024 idle waits (a `pop`
    /// attempt, the loads of `remaining` and `aborted`, a `spin_loop`)
    /// took 13.5 to 15.2 µs, the median of 2000 timed runs in each of three
    /// runs of a release build, about 15 ns a wait; on a 4-vCPU KVM guest
    /// of an Intel Xeon at 2.1 GHz with AVX-512 FP16 and AMX (Sapphire
    /// Rapids or later). A core whose `pause` is slower spins fewer waits in
    /// the same time. The clock starts at its first read, `CLOCK_EVERY`
    /// waits in, and is read that rarely, so a helper spins up to
    /// `2 * CLOCK_EVERY` waits (about 2 µs here) beyond the budget, and a
    /// budget of zero leaves after 128 idle waits.
    pub helper_leave_after: Duration,
}

impl BlockWaits {
    /// The defaults: 250 µs and 15 µs (see the fields).
    pub const DEFAULT: Self = Self {
        caller_yield_after: Duration::from_micros(250),
        helper_leave_after: Duration::from_micros(15),
    };
}

impl Default for BlockWaits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Waits between two reads of the clock. A wait is a `spin_loop` hint plus
/// the participant's own polling (a ready-list `pop` attempt and a load for
/// the task graph, a load for the gate). That takes from about 10 ns to
/// about 100 ns depending on the core and on contention: a `pause` is about
/// 14 ns on the VM measured in doc 013 and 50 to 70 ns on Intel since
/// Skylake. So the clock is read every 1 to 6 µs, which is fine-grained next
/// to the caller's budget. A read (`clock_gettime` through the vDSO) is
/// about 20 ns, so this costs well under 1% of a spin.
#[cfg(not(loom))]
pub(super) const CLOCK_EVERY: u32 = 64;

/// Under loom: the caller's waits without progress per yield.
#[cfg(loom)]
const LOOM_CALLER_SPINS: u32 = 2;

/// Under loom: the waits a helper spins before it leaves on the next.
#[cfg(loom)]
const LOOM_HELPER_SPINS: u32 = 2;

/// A wait that spins, then yields its core after
/// [`BlockWaits::caller_yield_after`] without progress (on Linux). See the
/// module docs (`src/rt/spin.rs`).
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
    /// How long the counter may stay unmoved.
    #[cfg(all(not(loom), any(target_os = "linux", target_os = "android")))]
    budget: Duration,
}

impl SpinThenYield {
    /// A wait on a counter now at `seen`, yielding after `budget` without
    /// progress (ignored where the caller only spins, and under loom).
    pub(super) fn new(seen: u32, budget: Duration) -> Self {
        #[cfg(any(loom, not(any(target_os = "linux", target_os = "android"))))]
        let _ = budget;
        Self {
            seen,
            #[cfg(any(loom, target_os = "linux", target_os = "android"))]
            spins: 0,
            #[cfg(all(not(loom), any(target_os = "linux", target_os = "android")))]
            since: None,
            #[cfg(all(not(loom), any(target_os = "linux", target_os = "android")))]
            budget,
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
        self.spins >= LOOM_CALLER_SPINS
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
            Some(s) => t.saturating_duration_since(s) >= self.budget,
        }
    }

    /// Off Linux the caller only spins (see the module docs, "Only on
    /// Linux").
    #[cfg(all(not(loom), not(any(target_os = "linux", target_os = "android"))))]
    fn due(&mut self) -> bool {
        false
    }
}

/// A helper's wait for work another participant is finishing: it spins
/// until [`BlockWaits::helper_leave_after`] has passed with nothing to take,
/// then says to leave. It never yields (`task_graph.rs`, "Waiting").
pub(super) struct SpinThenLeave {
    /// Waits since the last clock read (or, under loom, since the last
    /// task).
    spins: u32,
    /// When the helper was first seen idle, by the clock's reads; `None`
    /// until the first read after a task.
    #[cfg(not(loom))]
    since: Option<Instant>,
    /// How long it may stay idle.
    #[cfg(not(loom))]
    budget: Duration,
}

impl SpinThenLeave {
    /// A helper's wait, leaving after `budget` idle (a count under loom).
    pub(super) fn new(budget: Duration) -> Self {
        #[cfg(loom)]
        let _ = budget;
        Self {
            spins: 0,
            #[cfg(not(loom))]
            since: None,
            #[cfg(not(loom))]
            budget,
        }
    }

    /// The helper took a task: the budget starts again at its next wait.
    pub(super) fn reset(&mut self) {
        self.spins = 0;
        #[cfg(not(loom))]
        {
            self.since = None;
        }
    }

    /// One step of the wait: spins and returns `true`, or returns `false`
    /// once the budget is spent and the helper should leave.
    pub(super) fn wait(&mut self) -> bool {
        if self.spent() {
            return false;
        }
        // loom needs a yield to let another thread run.
        #[cfg(loom)]
        yield_now();
        #[cfg(not(loom))]
        spin_loop();
        true
    }

    #[cfg(loom)]
    fn spent(&mut self) -> bool {
        self.spins += 1;
        self.spins > LOOM_HELPER_SPINS
    }

    /// Every `CLOCK_EVERY` waits, a clock read; the first read after a task
    /// starts the clock.
    #[cfg(not(loom))]
    fn spent(&mut self) -> bool {
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
            Some(s) => t.saturating_duration_since(s) >= self.budget,
        }
    }
}

/// The clock: `Instant::now`, or a test's stepped clock on its thread.
#[cfg(not(loom))]
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
    /// so a wait's yield (or a helper's leaving) depends on how many waits
    /// it made and not on how long they took.
    pub(in crate::rt) fn step_clock(step: std::time::Duration) {
        CLOCK.with(|c| c.set(Some((std::time::Instant::now(), step))));
    }

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
        /// not before. Progress restarts the clock. The budget is the one the
        /// wait was given, not the default.
        ///
        /// Mutation: drop the restart on progress (`now != self.seen`) → the
        /// clock started before the progress keeps running, and the second
        /// segment yields early → the "progress restarts" assertion fails.
        /// Mutation: yield on every clock read past the first (ignore the
        /// elapsed time) → the first assertion counts a yield → fails.
        /// Mutation (run): compare against
        /// `BlockWaits::DEFAULT.caller_yield_after` instead of the wait's
        /// own budget → 250 µs is reached on the second read → fails.
        #[test]
        fn yields_once_the_clock_passes_the_budget() {
            static N: AtomicU32 = AtomicU32::new(0);
            count_yields_into(&N);
            let budget = std::time::Duration::from_millis(1);
            step_clock(budget / 4);
            let at = 5 * CLOCK_EVERY;
            let mut w = SpinThenYield::new(7, budget);
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
            let mut w = SpinThenYield::new(7, BlockWaits::DEFAULT.caller_yield_after);
            for _ in 0..100 * CLOCK_EVERY {
                w.wait(7);
            }
            assert_eq!(N.load(Ordering::Relaxed), 0);
        }
    }

    mod helper {
        use super::super::*;
        use super::*;

        /// With the clock stepping a quarter of the budget per read, a
        /// helper spins `5 * CLOCK_EVERY - 1` waits and leaves on the next,
        /// and a task taken in between starts the budget again. The budget is
        /// the one the wait was given, not the default.
        ///
        /// Mutations (run): leave on every clock read past the first (ignore
        /// the elapsed time) → leaves on wait `2 * CLOCK_EVERY` → fails;
        /// compare against `BlockWaits::DEFAULT.helper_leave_after` instead
        /// of the wait's own budget → 15 µs is reached on the third read →
        /// fails; drop `reset`'s clear of the start time → the second budget
        /// is spent at its first read → fails.
        #[test]
        fn a_helper_leaves_once_the_clock_passes_its_budget() {
            let budget = std::time::Duration::from_micros(40);
            step_clock(budget / 4);
            let at = 5 * CLOCK_EVERY;
            let mut w = SpinThenLeave::new(budget);
            for _ in 0..2 {
                for i in 0..at - 1 {
                    assert!(w.wait(), "inside the budget it spins (wait {i})");
                }
                assert!(!w.wait(), "then it leaves");
                w.reset();
            }
        }

        /// A helper whose clock never passes the budget never leaves,
        /// however many waits it makes: the budget is time, not a count.
        ///
        /// Mutation (run): leave on every clock read past the first, whatever
        /// it says → fails.
        #[test]
        fn a_helper_inside_its_budget_never_leaves() {
            step_clock(std::time::Duration::ZERO);
            let mut w = SpinThenLeave::new(BlockWaits::DEFAULT.helper_leave_after);
            for _ in 0..100 * CLOCK_EVERY {
                assert!(w.wait());
            }
        }
    }
}
