use super::*;

/// A diamond with a tail: 0 → {1, 2} → 3 → 4.
fn diamond() -> TaskGraph {
    TaskGraph::new(&[0, 1, 1, 2, 1], &[0, 2, 3, 4, 5, 5], &[1, 2, 3, 3, 4])
}

/// One participant runs every task once, in a topological order, block
/// after block (`begin` re-arms the counters).
///
/// Mutation: drop the counter store in `begin` → the second block's
/// counters start at 0 and wrap on the first decrement → nothing past
/// task 0 is ever ready → the watchdog aborts the second block → fails
/// (run: fails after 10 s).
#[test]
fn one_participant_runs_every_task_in_order() {
    let mut g = diamond();
    for _ in 0..3 {
        g.begin();
        let mut order = Vec::new();
        // A watchdog aborts a block that cannot finish, so a broken
        // reset fails here rather than hanging the run.
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        std::thread::scope(|s| {
            let g = &g;
            s.spawn(move || {
                if done_rx
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .is_err()
                {
                    g.abort();
                }
            });
            g.work(Participant::Caller, BlockWaits::DEFAULT, |t| order.push(t));
            let _ = done_tx.send(());
        });
        assert!(g.done(), "the block finished: {order:?}");
        let pos = |t: u32| order.iter().position(|&x| x == t).expect("ran");
        assert_eq!(order.len(), 5, "{order:?}");
        assert!(pos(0) < pos(1) && pos(0) < pos(2));
        assert!(pos(1) < pos(3) && pos(2) < pos(3) && pos(3) < pos(4));
    }
}

/// Several threads run every task exactly once, each after its
/// predecessors, over many blocks of a wide graph.
///
/// Mutation: in `pop`, skip the `EMPTY` check → a reserved slot is taken
/// before its store and `EMPTY` is run as a task → index out of bounds →
/// fails (under contention; the loom model catches it deterministically).
/// Mutation (run): drop the counter store in `begin` → the second block never
/// readies a successor, the watchdog aborts it after 10 s, and the run
/// assertion fails (it hung without the watchdog).
#[test]
fn threads_run_every_task_once() {
    use std::sync::atomic::AtomicU64;
    // 0 → 1..=32 → 33.
    let n = 34u32;
    let mut initial = vec![1u32; n as usize];
    initial[0] = 0;
    initial[33] = 32;
    let mut offsets = vec![0u32, 32];
    let mut targets: Vec<u32> = (1..=32).collect();
    for _ in 1..=32 {
        targets.push(33);
        offsets.push(targets.len() as u32);
    }
    offsets.push(targets.len() as u32);
    let mut g = TaskGraph::new(&initial, &offsets, &targets);
    for _ in 0..if cfg!(miri) { 4 } else { 200 } {
        g.begin();
        let runs: Vec<AtomicU64> = (0..n).map(|_| AtomicU64::new(0)).collect();
        let seen_all_middle = AtomicU64::new(0);
        // A watchdog aborts a block that cannot finish, so a broken reset
        // fails the assertions below rather than hanging the run.
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        std::thread::scope(|s| {
            s.spawn({
                let g = &g;
                move || {
                    if done_rx
                        .recv_timeout(std::time::Duration::from_secs(10))
                        .is_err()
                    {
                        g.abort();
                    }
                }
            });
            let workers: Vec<_> = (0..4)
                .map(|w| {
                    let (g, runs, seen) = (&g, &runs, &seen_all_middle);
                    s.spawn(move || {
                        let who = if w == 0 {
                            Participant::Caller
                        } else {
                            Participant::Helper(w)
                        };
                        g.work(who, BlockWaits::DEFAULT, |t| {
                            if t == 33 {
                                let done = (1..=32)
                                    .filter(|&m| {
                                        runs[m].load(std::sync::atomic::Ordering::Relaxed) == 1
                                    })
                                    .count();
                                seen.store(done as u64, std::sync::atomic::Ordering::Relaxed);
                            }
                            runs[t as usize].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        });
                    })
                })
                .collect();
            for worker in workers {
                worker.join().expect("a worker");
            }
            let _ = done_tx.send(());
        });
        assert!(
            runs.iter()
                .all(|r| r.load(std::sync::atomic::Ordering::Relaxed) == 1)
        );
        assert_eq!(
            seen_all_middle.load(std::sync::atomic::Ordering::Relaxed),
            32
        );
    }
}

/// A helper with nothing to take leaves the block after its spin budget,
/// while the caller is still inside a task: the caller never waits for a
/// helper that has nothing to do (so never for one the OS descheduled
/// while it yielded).
///
/// Mutation: make `SpinThenLeave::wait` return `true` past its budget
/// (the helper never leaves) → the helper stays until the block is done, which
/// waits on the caller's task, which waits on the helper → the
/// `recv_timeout` fails (run: fails after 10 s).
#[test]
fn an_idle_helper_leaves_the_block() {
    use std::sync::mpsc;
    use std::time::Duration;
    // 0 → 1: while the caller runs 0, nothing is ready.
    let mut g = TaskGraph::new(&[0, 1], &[0, 1, 1], &[1]);
    g.begin();
    let g = &g;
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let (left_tx, left_rx) = mpsc::channel();
    let helper_ran = std::thread::scope(|s| {
        let helper = s.spawn(move || {
            go_rx.recv().unwrap();
            let mut n = 0;
            g.work(Participant::Helper(1), BlockWaits::DEFAULT, |_| n += 1);
            left_tx.send(g.done()).unwrap();
            n
        });
        g.work(Participant::Caller, BlockWaits::DEFAULT, |t| {
            if t != 0 {
                return;
            }
            go_tx.send(()).unwrap();
            let Ok(done) = left_rx.recv_timeout(Duration::from_secs(10)) else {
                // Let the helper out, so the failure is a failure and
                // not a hang.
                g.abort();
                panic!("the helper stayed while the caller held task 0");
            };
            assert!(!done, "it left before the block was done");
        });
        helper.join().unwrap()
    });
    assert!(g.done());
    assert_eq!(helper_ran, 0, "nothing was ready while it waited");
}

/// The caller, with nothing to take while a helper holds the only task
/// in flight, yields its core once its budget passes with no task
/// finished: what lets a helper queued behind a `SCHED_FIFO` caller on
/// one core run at all (`src/spin.rs`). The caller's clock is a
/// stepped one (a quarter of the budget per read), and the helper holds
/// task 0 until it sees the caller's yield counted, so no timing is
/// involved; the 10 s bound is only how a missing yield fails rather
/// than hangs. Linux only, as the yield is.
///
/// Mutation: make `SpinThenYield::wait` spin on every wait (drop its
/// yield branch) → no yield is counted → the helper gives up after 10 s
/// and finishes the block → `saw_yield` is false → fails (run: fails
/// after 10 s).
#[test]
#[cfg(any(target_os = "linux", target_os = "android"))]
fn the_caller_yields_while_a_helper_holds_a_task() {
    use std::sync::atomic::AtomicU32;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    static YIELDED: AtomicU32 = AtomicU32::new(0);
    // 0 → 1: while the helper holds 0, the caller has nothing to take.
    let mut g = TaskGraph::new(&[0, 1], &[0, 1, 1], &[1]);
    g.begin();
    let g = &g;
    let (held_tx, held_rx) = mpsc::channel();
    let saw_yield = std::thread::scope(|s| {
        let helper = s.spawn(move || {
            let mut saw = false;
            g.work(Participant::Helper(1), BlockWaits::DEFAULT, |t| {
                if t != 0 {
                    return;
                }
                held_tx.send(()).unwrap();
                let deadline = Instant::now() + Duration::from_secs(10);
                while YIELDED.load(std::sync::atomic::Ordering::Acquire) == 0 {
                    if Instant::now() > deadline {
                        return;
                    }
                    std::thread::yield_now();
                }
                saw = true;
            });
            saw
        });
        held_rx.recv().unwrap();
        crate::spin::tests::count_yields_into(&YIELDED);
        crate::spin::tests::step_clock(BlockWaits::DEFAULT.caller_yield_after / 4);
        g.work(Participant::Caller, BlockWaits::DEFAULT, |t| {
            panic!("the caller took task {t}, which the helper holds or runs inline")
        });
        helper.join().unwrap()
    });
    assert!(
        saw_yield,
        "the caller never yielded while the helper held task 0"
    );
    assert!(g.done());
}

/// A helper spins for the budget it is given, not the default: with a
/// budget of an hour and its clock stepping the default budget per read,
/// it stays in the block while the caller holds task 0 for 50 ms, and
/// leaves only once the block is done. (A helper descheduled for those
/// 50 ms would pass this either way; it cannot fail spuriously.)
///
/// Mutation (run): build the helper's `SpinThenLeave` from
/// `BlockWaits::DEFAULT` instead of `waits` → it leaves on its second
/// clock read, with the block not done → fails.
#[test]
fn a_helper_spins_for_the_pools_budget() {
    use std::sync::mpsc;
    use std::time::Duration;
    let waits = BlockWaits {
        helper_leave_after: Duration::from_secs(3600),
        ..BlockWaits::DEFAULT
    };
    // 0 → 1: while the caller runs 0, nothing is ready.
    let mut g = TaskGraph::new(&[0, 1], &[0, 1, 1], &[1]);
    g.begin();
    let g = &g;
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let (left_tx, left_rx) = mpsc::channel();
    std::thread::scope(|s| {
        s.spawn(move || {
            go_rx.recv().unwrap();
            crate::spin::tests::step_clock(BlockWaits::DEFAULT.helper_leave_after);
            g.work(Participant::Helper(1), waits, |_| {});
            left_tx.send(g.done()).unwrap();
        });
        g.work(Participant::Caller, waits, |t| {
            if t != 0 {
                return;
            }
            go_tx.send(()).unwrap();
            if let Ok(done) = left_rx.recv_timeout(Duration::from_millis(50)) {
                assert!(done, "the helper left with the block not done");
            }
        });
    });
    assert!(g.done());
}

/// The caller yields after the budget it is given, not the default: with
/// a budget of an hour and its clock stepping the default budget per
/// read, it waits 50 ms on a helper holding task 0 without yielding.
/// Linux only, as the yield is.
///
/// Mutation (run): build the caller's `SpinThenYield` from
/// `BlockWaits::DEFAULT` instead of `waits` → it yields on its second
/// clock read → fails.
#[test]
#[cfg(any(target_os = "linux", target_os = "android"))]
fn the_caller_yields_after_the_pools_budget() {
    use std::sync::atomic::AtomicU32;
    use std::sync::mpsc;
    use std::time::Duration;
    static YIELDED: AtomicU32 = AtomicU32::new(0);
    let waits = BlockWaits {
        caller_yield_after: Duration::from_secs(3600),
        ..BlockWaits::DEFAULT
    };
    let mut g = TaskGraph::new(&[0, 1], &[0, 1, 1], &[1]);
    g.begin();
    let g = &g;
    let (held_tx, held_rx) = mpsc::channel();
    std::thread::scope(|s| {
        s.spawn(move || {
            g.work(Participant::Helper(1), waits, |t| {
                if t == 0 {
                    held_tx.send(()).unwrap();
                    std::thread::sleep(Duration::from_millis(50));
                }
            });
        });
        held_rx.recv().unwrap();
        crate::spin::tests::count_yields_into(&YIELDED);
        crate::spin::tests::step_clock(BlockWaits::DEFAULT.caller_yield_after);
        g.work(Participant::Caller, waits, |t| {
            panic!("the caller took task {t}, which the helper holds or runs inline")
        });
    });
    assert!(g.done());
    assert_eq!(YIELDED.load(std::sync::atomic::Ordering::Acquire), 0);
}

/// Every task a helper finishes, sink or not, moves its completion
/// counter (slot `i % PROGRESS_SLOTS` for helper `i`), and the caller's
/// own tasks touch no counter: what the waiting caller reads as
/// progress, since `remaining` moves only for sinks.
///
/// Mutations (run): the helper's store dropped → fails (0, not 5); the
/// slot taken as `0` whatever the helper → fails (slot 1 unmoved).
#[test]
fn a_helpers_every_task_moves_its_completion_counter() {
    let slot = |g: &TaskGraph, i: usize| g.progress[i].0.load(Ordering::Relaxed);
    let mut g = diamond();
    g.begin();
    g.work(
        Participant::Helper(PROGRESS_SLOTS + 1),
        BlockWaits::DEFAULT,
        |_| {},
    );
    assert!(g.done());
    assert_eq!(slot(&g, 1), 5, "five tasks, one of them a sink");
    g.begin();
    g.work(Participant::Caller, BlockWaits::DEFAULT, |_| {});
    assert!((0..PROGRESS_SLOTS).all(|i| slot(&g, i) == if i == 1 { 5 } else { 0 }));
}

/// An abort makes every participant leave with work pending.
///
/// Mutation: drop the `aborted` check in `work` → the participant keeps
/// running the chain after the abort → fails.
#[test]
fn an_abort_ends_the_block() {
    let mut g = diamond();
    g.begin();
    let mut ran = 0;
    g.work(Participant::Caller, BlockWaits::DEFAULT, |_| {
        ran += 1;
        g.abort();
    });
    assert_eq!(ran, 1);
    assert!(!g.done());
    g.begin();
    assert!(!g.is_aborted());
    let mut ran = 0;
    g.work(Participant::Caller, BlockWaits::DEFAULT, |_| ran += 1);
    assert_eq!(ran, 5);
}

/// A cyclic graph is refused: its tasks would wait on each other forever.
///
/// Mutation: drop the Kahn pass in `new` → built → the `should_panic`
/// fails.
#[test]
#[should_panic(expected = "cycle")]
fn a_cycle_is_refused() {
    // 0 → 1 → 2 → 1.
    let _ = TaskGraph::new(&[0, 2, 1], &[0, 1, 2, 3], &[1, 2, 1]);
}

/// A graph whose counts disagree with its rows is refused.
///
/// Mutation: drop the predecessor-count `assert_eq!` in `new` → built,
/// and task 1 would never run → the `should_panic` fails.
#[test]
#[should_panic(expected = "disagree")]
fn inconsistent_counts_are_refused() {
    let _ = TaskGraph::new(&[0, 2], &[0, 1, 1], &[1]);
}

/// **`begin` re-arms every counter.** A lone helper with a short budget runs
/// block after block: a counter left at its last value (zero) readies no
/// successor, the helper finds nothing to take and leaves, and the block is
/// short of tasks. A helper never leaves a healthy block here (the next task
/// is always ready), so this takes milliseconds either way.
///
/// Mutation (run): drop the counter store in `begin` → the second block runs
/// task 0 and the helper leaves with the other four undone → fails at once.
#[test]
fn begin_re_arms_the_counters_for_the_next_block() {
    let waits = BlockWaits {
        helper_leave_after: std::time::Duration::from_millis(5),
        ..BlockWaits::DEFAULT
    };
    let mut g = diamond();
    for block in 0..3 {
        g.begin();
        let mut order = Vec::new();
        g.work(Participant::Helper(1), waits, |t| order.push(t));
        assert_eq!(order.len(), 5, "block {block}: {order:?}");
        assert!(g.done(), "block {block} finished");
    }
}
