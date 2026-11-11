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
pub(crate) fn count_yields_into(counter: &'static AtomicU32) {
    YIELDS.with(|y| y.set(Some(counter)));
}

/// Replace this thread's clock with one that advances `step` per read,
/// so a wait's yield (or a helper's leaving) depends on how many waits
/// it made and not on how long they took.
pub(crate) fn step_clock(step: std::time::Duration) {
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
    #[test]
    fn a_helper_inside_its_budget_never_leaves() {
        step_clock(std::time::Duration::ZERO);
        let mut w = SpinThenLeave::new(BlockWaits::DEFAULT.helper_leave_after);
        for _ in 0..100 * CLOCK_EVERY {
            assert!(w.wait());
        }
    }
}
