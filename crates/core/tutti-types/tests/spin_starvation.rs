//! A scheduler probe, not a CI test: the audio thread and one pool helper
//! pinned to the **same core**, the helper holding the only task in flight,
//! and the audio thread ([`Participant::Caller`]) waiting for it in
//! [`TaskGraph::work`].
//!
//! ```text
//! cargo test -p tutti-types --release --test spin_starvation -- --ignored --nocapture
//! ```
//!
//! Linux only. The `SCHED_FIFO` cases need `CAP_SYS_NICE` (or an `rtprio`
//! limit); without it they print that they were skipped. Each FIFO case
//! occupies one core at RT priority for up to the watchdog's 2 s (the
//! kernel's RT throttling caps that at `sched_rt_runtime_us`, 950 ms in every
//! second by default), so do not run it on a machine doing real-time work.
//!
//! What it measures, per case: the caller's time in `work` while the helper
//! holds task 0 of `0 → 1`. The task burns about 1 ms of CPU, blocks for
//! 1 ms (as a node that touches a page not yet in memory, or takes a lock,
//! would), and burns 1 ms more. The caller enters `work` once the helper
//! holds the task, so the helper comes back from its block to a core the
//! caller is spinning on. Unstarved, the wait is at most the task's 3 ms.
//!
//! - **`OTHER` caller, `OTHER` helper.** The fair scheduler time-slices
//!   them, so the helper finishes in slices between the caller's.
//! - **`FIFO` caller, `OTHER` helper.** The helper runs only when the RT
//!   class lets go of the core (RT throttling, or the fair server on 6.12+
//!   kernels). A yield does not help: `sched_yield` from a FIFO thread
//!   requeues it within its own priority, ahead of every `OTHER` thread.
//!   This is why the pool's helpers must run at the caller's priority
//!   (`tutti-core`'s `pool.rs`, "Priority").
//! - **`FIFO` caller, `FIFO` helper at the same priority.** FIFO does not
//!   time-slice, so a caller that only spins keeps the helper off the core
//!   for good once it is queued behind it (here, after its block): the
//!   watchdog has to abort the block. A caller that yields after 250 µs
//!   without progress (`rt/spin.rs`) goes behind the helper, which finishes
//!   the task, and the wait is about the rest of the task plus that budget.
//!   This case is asserted.
//!
//! A run whose caller found the task already finished (its wait under
//! 1 ms: the helper ran its whole task before the caller's 20 µs sleep woke,
//! which an equal-priority FIFO helper can do) did not exercise the wait. It
//! is reported as such, and the assertion needs at least one run that did.
//!
//! In a real deployment the helper is not pinned, and the kernel's RT
//! push/pull migrates a runnable FIFO helper to a core running anything of
//! lower priority, so the stall needs every other core busy at or above the
//! audio thread's priority. The probe removes the other cores to show what
//! happens then.

#![cfg(all(target_os = "linux", not(loom), not(miri)))]

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tutti_types::{Participant, TaskGraph};

#[derive(Clone, Copy, Debug, PartialEq)]
enum Policy {
    Other,
    Fifo(i32),
}

fn pin(cpu: usize) {
    // SAFETY: a zeroed `cpu_set_t` is the empty set; the calls only read it.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        let r = libc::sched_setaffinity(0, std::mem::size_of_val(&set), &set);
        assert_eq!(r, 0, "pin to cpu {cpu}");
    }
}

/// Set this thread's policy; `false` when not permitted.
fn set_policy(p: Policy) -> bool {
    let (policy, prio) = match p {
        Policy::Other => (libc::SCHED_OTHER, 0),
        Policy::Fifo(prio) => (libc::SCHED_FIFO, prio),
    };
    let param = libc::sched_param {
        sched_priority: prio,
    };
    // SAFETY: the calling thread's own handle and a valid parameter.
    unsafe { libc::pthread_setschedparam(libc::pthread_self(), policy, &param) == 0 }
}

fn burn(iters: u64) {
    for i in 0..iters {
        std::hint::black_box(i);
        std::hint::spin_loop();
    }
}

/// Iterations of `burn` that take about `target` of CPU on `cpu`, alone.
fn calibrate(cpu: usize, target: Duration) -> u64 {
    std::thread::scope(|s| {
        s.spawn(|| {
            pin(cpu);
            let probe = 1u64 << 18;
            burn(probe);
            let t = Instant::now();
            burn(probe);
            let per = t.elapsed().as_secs_f64() / probe as f64;
            (target.as_secs_f64() / per) as u64
        })
        .join()
        .unwrap()
    })
}

struct Outcome {
    wait: Duration,
    aborted: bool,
}

/// Below this, the caller found task 0 already finished: the run did not
/// exercise the wait. An exercised wait is at least the task's last 1 ms of
/// CPU.
const EXERCISED: Duration = Duration::from_millis(1);

impl Outcome {
    fn exercised(&self) -> bool {
        self.aborted || self.wait >= EXERCISED
    }
}

/// The caller's wait while a same-core helper holds task 0, or `None` if a
/// policy was not permitted.
fn measure(cpu: usize, caller: Policy, helper: Policy, iters: u64) -> Option<Outcome> {
    let mut g = TaskGraph::new(&[0, 1], &[0, 1, 1], &[1]);
    g.begin();
    let g = &g;
    let holding = AtomicBool::new(false);
    let refused = AtomicBool::new(false);
    let finished = AtomicBool::new(false);
    let aborted = AtomicBool::new(false);
    let wait = std::thread::scope(|s| {
        let (holding, refused, finished, aborted) = (&holding, &refused, &finished, &aborted);
        s.spawn(move || {
            pin(cpu);
            if !set_policy(helper) {
                refused.store(true, Ordering::Release);
                g.abort();
                return;
            }
            g.work(Participant::Helper, |t| {
                if t == 0 {
                    holding.store(true, Ordering::Release);
                    burn(iters);
                    std::thread::sleep(Duration::from_millis(1));
                    burn(iters);
                }
            });
        });
        // The watchdog, on another core. On a one-CPU machine that is the
        // pair's own core, and the watchdog (`OTHER`) runs only once RT
        // throttling takes the core from the FIFO pair: its abort comes
        // late, but it comes.
        s.spawn(move || {
            pin((cpu + 1) % std::thread::available_parallelism().map_or(1, |n| n.get()));
            let deadline = Instant::now() + Duration::from_secs(2);
            while !finished.load(Ordering::Acquire) {
                if Instant::now() > deadline {
                    aborted.store(true, Ordering::Release);
                    g.abort();
                    return;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        let caller = s.spawn(move || {
            pin(cpu);
            if !set_policy(caller) {
                refused.store(true, Ordering::Release);
                g.abort();
                return Duration::ZERO;
            }
            // Sleep, not spin, so the helper gets the core to take task 0.
            while !holding.load(Ordering::Acquire) && !g.aborted() {
                std::thread::sleep(Duration::from_micros(20));
            }
            let t = Instant::now();
            g.work(Participant::Caller, |_| {});
            let wait = t.elapsed();
            // Back to `OTHER` before the scope joins the others.
            set_policy(Policy::Other);
            wait
        });
        let wait = caller.join().unwrap();
        finished.store(true, Ordering::Release);
        wait
    });
    if refused.load(Ordering::Acquire) {
        return None;
    }
    Some(Outcome {
        wait,
        aborted: aborted.load(Ordering::Acquire),
    })
}

#[test]
#[ignore = "a scheduler probe: pins threads to one core and uses SCHED_FIFO; run by hand"]
fn same_core_caller_and_helper() {
    let cpu = 0;
    let iters = calibrate(cpu, Duration::from_millis(1));
    println!("task 0: 2 x {iters} iterations (about 1 ms of CPU each) around a 1 ms sleep");
    let cases = [
        ("OTHER caller, OTHER helper", Policy::Other, Policy::Other),
        ("FIFO caller, OTHER helper", Policy::Fifo(10), Policy::Other),
        (
            "FIFO caller, FIFO helper (same priority)",
            Policy::Fifo(10),
            Policy::Fifo(10),
        ),
    ];
    let mut fifo_equal = Vec::new();
    for (name, caller, helper) in cases {
        let mut runs = Vec::new();
        for _ in 0..5 {
            match measure(cpu, caller, helper, iters) {
                Some(o) => runs.push(o),
                None => break,
            }
            if caller != Policy::Other {
                // Let the RT budget refill, so one run's throttling does not
                // delay the next one's caller.
                std::thread::sleep(Duration::from_millis(1100));
            }
        }
        if runs.is_empty() {
            println!("{name}: skipped (SCHED_FIFO not permitted: no CAP_SYS_NICE)");
            continue;
        }
        let waits: Vec<_> = runs.iter().map(|o| o.wait).collect();
        let aborts = runs.iter().filter(|o| o.aborted).count();
        let idle = runs.iter().filter(|o| !o.exercised()).count();
        println!(
            "{name}: caller waits {waits:?}; {aborts}/{} aborted by the 2 s watchdog, \
             {idle} not exercised (wait under {EXERCISED:?})",
            runs.len()
        );
        if caller == helper {
            if let Policy::Fifo(_) = caller {
                fifo_equal = runs;
            }
        }
    }
    // The fix's property: at equal FIFO priority on one core, the caller's
    // yield lets the helper finish, so no block needs the watchdog; and at
    // least one run exercised the wait, so the property was tested.
    //
    // Mutation: make the caller never yield (`SpinThenYield::wait` spins
    // only) → every exercised FIFO/FIFO run is aborted at 2 s → fails (run:
    // 4/5 aborted at ≈ 2.0 s, the fifth not exercised). Mutation:
    // drop the helper's mid-task sleep → an equal-priority FIFO caller cannot
    // run until the helper has finished the task → no run is exercised →
    // fails (run: 5/5 not exercised, waits 113–200 ns).
    if !fifo_equal.is_empty() {
        assert!(
            fifo_equal.iter().all(|o| !o.aborted),
            "a same-priority FIFO caller starved its helper until the watchdog"
        );
        assert!(
            fifo_equal.iter().any(Outcome::exercised),
            "no FIFO/FIFO run exercised the wait: every caller found the task done"
        );
    }
}
