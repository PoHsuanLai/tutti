use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use super::*;

/// Helpers join with distinct indices, and `run` returns only once they
/// have left.
#[test]
fn helpers_join_and_the_caller_waits_for_them() {
    let gate = Arc::new(JobGate::new(3));
    let stop = Arc::new(AtomicBool::new(false));
    let helpers: Vec<_> = (0..2)
        .map(|_| {
            let (gate, stop) = (Arc::clone(&gate), Arc::clone(&stop));
            std::thread::spawn(move || {
                let mut served = 0;
                while !stop.load(Ordering::Relaxed) {
                    gate.help(&mut served);
                    std::hint::spin_loop();
                }
            })
        })
        .collect();
    for _ in 0..if cfg!(miri) { 4 } else { 200 } {
        let hits = AtomicUsize::new(0);
        let mask = AtomicUsize::new(0);
        let joined = gate.run(
            &|i| {
                mask.fetch_or(1 << i, Ordering::Relaxed);
                // Hold participant 0 long enough for helpers to join.
                if i == 0 {
                    for _ in 0..if cfg!(miri) { 20 } else { 2_000 } {
                        std::hint::spin_loop();
                    }
                }
                hits.fetch_add(1, Ordering::Relaxed);
            },
            || {},
        );
        assert!(joined);
        let m = mask.load(Ordering::Relaxed);
        assert_eq!(hits.load(Ordering::Relaxed), m.count_ones() as usize);
        assert!(m & 1 == 1 && m < 8);
    }
    stop.store(true, Ordering::Relaxed);
    for h in helpers {
        h.join().expect("helper");
    }
}

/// The close, waiting for a helper still inside the job, yields the
/// caller's core once its budget passes with `entered` unchanged: what
/// lets a helper queued behind a `SCHED_FIFO` caller on one core leave
/// at all (`src/spin.rs`). The caller's clock is a stepped one (a
/// quarter of the budget per read), and the helper stays inside until it
/// sees the caller's yield counted, so no timing is involved; the 10 s
/// bound is only how a missing yield fails rather than hangs. Linux
/// only, as the yield is.
#[test]
#[cfg(any(target_os = "linux", target_os = "android"))]
fn the_close_yields_while_a_helper_is_inside() {
    use std::time::{Duration, Instant};
    static YIELDED: AtomicU32 = AtomicU32::new(0);
    let gate = JobGate::new(2);
    let inside = AtomicBool::new(false);
    let saw = AtomicBool::new(false);
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| {
            let mut served = 0;
            while !stop.load(Ordering::Acquire) && !gate.help(&mut served) {
                std::hint::spin_loop();
            }
        });
        crate::spin::tests::count_yields_into(&YIELDED);
        crate::spin::tests::step_clock(BlockWaits::DEFAULT.caller_yield_after / 4);
        let joined = gate.run(
            &|i| {
                let deadline = Instant::now() + Duration::from_secs(10);
                if i == 0 {
                    // Hold participant 0 until the helper is inside, so
                    // the close has a helper to wait for.
                    while !inside.load(Ordering::Acquire) && Instant::now() < deadline {
                        std::thread::yield_now();
                    }
                } else {
                    inside.store(true, Ordering::Release);
                    while YIELDED.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
                        std::thread::yield_now();
                    }
                    saw.store(YIELDED.load(Ordering::Acquire) > 0, Ordering::Relaxed);
                }
            },
            || {},
        );
        assert!(joined);
        stop.store(true, Ordering::Release);
    });
    assert!(inside.load(Ordering::Relaxed), "the helper joined");
    assert!(
        saw.load(Ordering::Relaxed),
        "the close never yielded while the helper was inside"
    );
}

/// The close yields after the gate's own budget, not the default: with
/// a budget of an hour and its clock stepping the default budget per
/// read, it waits 50 ms on a helper inside without yielding. Linux only,
/// as the yield is.
#[test]
#[cfg(any(target_os = "linux", target_os = "android"))]
fn the_close_yields_after_the_gates_budget() {
    use std::time::Duration;
    static YIELDED: AtomicU32 = AtomicU32::new(0);
    let waits = BlockWaits {
        caller_yield_after: Duration::from_secs(3600),
        ..BlockWaits::DEFAULT
    };
    let gate = JobGate::with_waits(2, waits);
    assert_eq!(gate.waits(), waits);
    let inside = AtomicBool::new(false);
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| {
            let mut served = 0;
            while !stop.load(Ordering::Acquire) && !gate.help(&mut served) {
                std::hint::spin_loop();
            }
        });
        crate::spin::tests::count_yields_into(&YIELDED);
        crate::spin::tests::step_clock(BlockWaits::DEFAULT.caller_yield_after);
        let joined = gate.run(
            &|i| {
                if i == 0 {
                    // Hold participant 0 until the helper is inside, so
                    // the close has a helper to wait for.
                    let deadline = std::time::Instant::now() + Duration::from_secs(10);
                    while !inside.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
                        std::thread::yield_now();
                    }
                } else {
                    inside.store(true, Ordering::Release);
                    std::thread::sleep(Duration::from_millis(50));
                }
            },
            || {},
        );
        assert!(joined);
        stop.store(true, Ordering::Release);
    });
    assert!(inside.load(Ordering::Relaxed), "the helper joined");
    assert_eq!(YIELDED.load(Ordering::Acquire), 0);
}

/// A call from inside a job runs alone rather than waiting on itself.
#[test]
fn a_nested_call_runs_alone() {
    let gate = JobGate::new(2);
    let inner = AtomicUsize::new(0);
    assert!(gate.run(
        &|_| {
            assert!(!gate.run(
                &|i| {
                    inner.fetch_add(1 + i, Ordering::Relaxed);
                },
                || {}
            ));
        },
        || {}
    ));
    assert_eq!(inner.load(Ordering::Relaxed), 1);
}

/// A busy gate refuses `try_run` without calling the job, and a free
/// one runs it.
#[test]
fn try_run_on_a_busy_gate_runs_nothing() {
    let gate = JobGate::new(2);
    let inner = AtomicUsize::new(0);
    assert!(gate.try_run(
        &|_| {
            assert!(!gate.try_run(
                &|_| {
                    inner.fetch_add(1, Ordering::Relaxed);
                },
                || {}
            ));
        },
        || {}
    ));
    assert_eq!(inner.load(Ordering::Relaxed), 0);
    assert!(gate.try_run(
        &|_| {
            inner.fetch_add(1, Ordering::Relaxed);
        },
        || {}
    ));
    assert_eq!(inner.load(Ordering::Relaxed), 1);
}

/// A panicking job still closes the gate, so the next call can run.
#[test]
fn a_panicking_job_closes_the_gate() {
    let gate = JobGate::new(2);
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        gate.run(&|_| panic!("boom"), || {});
    }));
    assert!(r.is_err());
    assert!(gate.run(&|_| {}, || {}));
}

/// **`run` returns only after a helper inside the job has left it.** The
/// helper stays in the job for 20 ms once the caller's share is done, then
/// records that it left. Both waits are bounded, so a failure is an
/// assertion rather than a hang.
#[test]
fn run_waits_for_a_helper_still_inside() {
    use std::time::{Duration, Instant};
    let gate = JobGate::new(2);
    let inside = AtomicBool::new(false);
    let left = AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| {
            let mut served = 0;
            let deadline = Instant::now() + Duration::from_secs(10);
            while !gate.help(&mut served) && Instant::now() < deadline {
                std::hint::spin_loop();
            }
        });
        let joined = gate.run(
            &|i| {
                if i == 0 {
                    // Hold participant 0 until the helper is inside.
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while !inside.load(Ordering::Acquire) && Instant::now() < deadline {
                        std::thread::yield_now();
                    }
                } else {
                    inside.store(true, Ordering::Release);
                    std::thread::sleep(Duration::from_millis(20));
                    left.store(true, Ordering::Release);
                }
            },
            || {},
        );
        assert!(joined);
        assert!(inside.load(Ordering::Acquire), "the helper joined");
        assert!(
            left.load(Ordering::Acquire),
            "run returned while the helper was still inside the job"
        );
    });
}

/// **Every generation hands a helper an index**, not only the first: `run`
/// restarts the index counter. The helper is the caller's own thread, calling
/// `help` from inside participant 0's share, so no timing is involved.
#[test]
fn every_generation_hands_a_helper_an_index() {
    use std::sync::Mutex;
    let gate = JobGate::new(2);
    let served = Mutex::new(0u64);
    for generation in 0..3 {
        let mask = AtomicUsize::new(0);
        assert!(gate.run(
            &|i| {
                mask.fetch_or(1 << i, Ordering::Relaxed);
                if i == 0 {
                    let helped = gate.help(&mut served.lock().unwrap());
                    assert!(helped, "generation {generation}: the helper ran no share");
                }
            },
            || {},
        ));
        assert_eq!(
            mask.load(Ordering::Relaxed),
            0b11,
            "generation {generation}"
        );
    }
}
