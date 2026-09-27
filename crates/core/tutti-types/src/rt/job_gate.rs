//! [`JobGate`]: hand a borrowed job to helper threads for one call, and know
//! when every helper that took it has let go.
//!
//! A worker pool runs a closure that borrows the caller's stack (an
//! executor's per-block state) on threads that outlive it. The safe
//! spelling, `std::thread::scope`, spawns threads per call, which the audio
//! thread cannot do. This is the same guarantee without spawning: the gate
//! stores the job's address with its lifetime erased, and
//! [`run`](JobGate::run) does not return (or unwind) until the gate is
//! closed and no helper is inside the job, so the address is never used
//! after the borrow ends. The erasure is the only `unsafe` here, and the
//! `loom` model in `tests/job_gate_loom.rs` checks the protocol that makes
//! it sound against the shipped code.
//!
//! # The protocol
//!
//! `state` holds a generation number and an open bit. The caller:
//!
//! 1. takes the gate (`busy`; a caller that finds it taken — another
//!    executor, or a re-entrant call from inside a job — runs the job alone
//!    as participant 0 and returns);
//! 2. writes the job, then opens the next generation (`Release`);
//! 3. runs the job as participant 0;
//! 4. closes the gate, issues a `SeqCst` fence, then waits (`Acquire`) until
//!    `entered` is zero.
//!
//! A helper increments `entered`, issues a `SeqCst` fence, *then* re-reads
//! `state`: only if it still shows the open generation it saw does it read
//! the job. The two fences are totally ordered, so a helper that the
//! caller's wait did not see has seen the gate closed, and reads nothing (a
//! store-load Dekker pair). Fences rather than `SeqCst` accesses because a
//! fence is what `loom` models exactly (it treats `SeqCst` accesses as
//! `AcqRel`, under which the model finds the reordering), and it is what
//! `RtPublish` uses for its own store-load pair. A helper serves each
//! generation at most once.
//!
//! The caller never waits on a helper that has not started: a helper still
//! asleep when the gate closes never enters. It waits only for helpers
//! inside the job, which leave as soon as the job's own termination says so.

use sync::{fence, AtomicBool, AtomicU32, AtomicU64, Ordering, UnsafeCell};

mod sync {
    #[cfg(loom)]
    pub(super) use loom::cell::UnsafeCell;
    #[cfg(loom)]
    pub(super) use loom::sync::atomic::{fence, AtomicBool, AtomicU32, AtomicU64, Ordering};
    #[cfg(not(loom))]
    pub(super) use std::sync::atomic::{fence, AtomicBool, AtomicU32, AtomicU64, Ordering};

    /// `loom`'s `UnsafeCell` API over `std`'s, so the gate is written once.
    #[cfg(not(loom))]
    pub(super) struct UnsafeCell<T>(std::cell::UnsafeCell<T>);
    #[cfg(not(loom))]
    impl<T> UnsafeCell<T> {
        pub(super) const fn new(v: T) -> Self {
            Self(std::cell::UnsafeCell::new(v))
        }
        pub(super) fn with<R>(&self, f: impl FnOnce(*const T) -> R) -> R {
            f(self.0.get())
        }
        pub(super) fn with_mut<R>(&self, f: impl FnOnce(*mut T) -> R) -> R {
            f(self.0.get())
        }
    }
}

/// The job, with its lifetime erased. Only ever dereferenced between a
/// helper's successful entry and its exit, both inside one `run`.
type Job = *const (dyn Fn(usize) + Sync + 'static);

/// A gate that lends one borrowed job at a time to helper threads. See the
/// module docs (`src/rt/job_gate.rs`).
pub struct JobGate {
    /// `generation << 1 | open`.
    state: AtomicU64,
    /// Helpers inside the current generation (or about to find it closed).
    entered: AtomicU32,
    /// The next participant index to hand a helper.
    next: AtomicU32,
    /// Participants a job is run by at most: the caller and the helpers.
    participants: u32,
    busy: AtomicBool,
    job: UnsafeCell<Option<Job>>,
}

// SAFETY: the job cell is written only by the caller holding `busy`, while
// the gate is closed and `entered` is zero (so no helper reads it), and read
// by helpers only between a successful entry and their exit, which the
// caller waits for before writing again (module docs). The job itself is
// `Sync`, so calling it from several threads at once is allowed.
unsafe impl Sync for JobGate {}
// SAFETY: nothing in the gate is tied to a thread.
unsafe impl Send for JobGate {}

impl JobGate {
    /// A closed gate for jobs run by at most `participants` threads (the
    /// caller and `participants - 1` helpers).
    ///
    /// # Panics
    ///
    /// If `participants` is zero.
    pub fn new(participants: usize) -> Self {
        assert!(participants > 0, "a job needs its caller");
        Self {
            state: AtomicU64::new(0),
            entered: AtomicU32::new(0),
            next: AtomicU32::new(1),
            participants: u32::try_from(participants).expect("participant count"),
            busy: AtomicBool::new(false),
            job: UnsafeCell::new(None),
        }
    }

    /// The most participants a job can have.
    pub fn participants(&self) -> usize {
        self.participants as usize
    }

    /// Run `job` on this thread as participant 0 and on every helper that
    /// [`help`](Self::help)s before it ends, each with its own index in
    /// `1..participants`. `opened` runs right after the gate opens (a pool
    /// wakes its sleeping helpers there). Returns once no helper is inside
    /// `job`, and whether helpers could join: `false` if the gate was busy
    /// (another caller holds it, or this is a call from inside a job), in
    /// which case `job(0)` ran alone.
    ///
    /// Never waits for a helper that has not entered. If `job` panics, the
    /// gate still closes and waits for the helpers inside before the panic
    /// continues.
    pub fn run(&self, job: &(dyn Fn(usize) + Sync), opened: impl FnOnce()) -> bool {
        if self
            .busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            job(0);
            return false;
        }
        // SAFETY: only the lifetime changes. The pointer is read by helpers
        // only while the gate is open for this generation or while they are
        // counted in `entered`; `Close` below closes the gate and waits for
        // `entered` to reach zero before this function returns or unwinds,
        // so no helper dereferences it after `job`'s borrow ends.
        let erased: Job = unsafe {
            std::mem::transmute::<*const (dyn Fn(usize) + Sync + '_), Job>(
                job as *const (dyn Fn(usize) + Sync),
            )
        };
        // SAFETY: we hold `busy`, the gate is closed and `entered` was zero
        // when the previous holder released `busy` (its `Close`), and a
        // helper reads the cell only after seeing it open: nobody reads it
        // now.
        self.job.with_mut(|p| unsafe { *p = Some(erased) });
        self.next.store(1, Ordering::Relaxed);
        let open = (self.state.load(Ordering::Relaxed) | 1) + 2;
        self.state.store(open, Ordering::Release);
        let _close = Close(self);
        opened();
        job(0);
        true
    }

    /// Whether a generation this helper has not served (its last served one
    /// is `*served`) is open — a cheap check for a helper's wait loop.
    pub fn pending(&self, served: u64) -> bool {
        let s = self.state.load(Ordering::Acquire);
        s & 1 == 1 && s >> 1 != served
    }

    /// As a helper, take part in the open generation if there is one this
    /// helper has not served: run the job with a participant index, then
    /// leave. `served` is this helper's own record of the last generation it
    /// served (start it at 0). Returns whether it ran the job.
    pub fn help(&self, served: &mut u64) -> bool {
        let s = self.state.load(Ordering::Acquire);
        if s & 1 == 0 || s >> 1 == *served {
            return false;
        }
        self.entered.fetch_add(1, Ordering::Relaxed);
        let _leave = Leave(self);
        // The store-load pair with `Close::drop` (module docs): either the
        // caller's wait sees this increment, or this load sees the gate
        // closed.
        fence(Ordering::SeqCst);
        if self.state.load(Ordering::Acquire) != s {
            return false;
        }
        *served = s >> 1;
        let idx = self.next.fetch_add(1, Ordering::Relaxed);
        if idx >= self.participants {
            return false;
        }
        // SAFETY: the gate is open for generation `s >> 1` and this helper is
        // counted in `entered` (the store-load pair in the module docs), so
        // the caller wrote the cell before opening and will not write it again
        // or return until this helper leaves.
        let job = self.job.with(|p| unsafe { *p });
        let job = job.expect("an open gate holds its job");
        // SAFETY: as above: the job's borrow outlives this call.
        unsafe { (*job)(idx as usize) };
        true
    }
}

impl std::fmt::Debug for JobGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobGate")
            .field("participants", &self.participants)
            .finish()
    }
}

/// Closes the gate and waits out the helpers inside, on return or unwind.
struct Close<'g>(&'g JobGate);

impl Drop for Close<'_> {
    fn drop(&mut self) {
        let g = self.0;
        let open = g.state.load(Ordering::Relaxed);
        g.state.store(open & !1, Ordering::Relaxed);
        // The other half of `help`'s store-load pair.
        fence(Ordering::SeqCst);
        // `Acquire`: every helper's use of the job happens-before its exit's
        // `Release`, and so before this returns.
        while g.entered.load(Ordering::Acquire) != 0 {
            #[cfg(loom)]
            loom::thread::yield_now();
            #[cfg(not(loom))]
            std::hint::spin_loop();
        }
        // Nobody reads the cell now; clear it so no stale address lingers.
        // SAFETY: closed, and `entered` is zero.
        g.job.with_mut(|p| unsafe { *p = None });
        g.busy.store(false, Ordering::Release);
    }
}

/// A helper's exit, on return or unwind.
struct Leave<'g>(&'g JobGate);

impl Drop for Leave<'_> {
    fn drop(&mut self) {
        self.0.entered.fetch_sub(1, Ordering::Release);
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    use super::*;

    /// Helpers join with distinct indices, and `run` returns only once they
    /// have left.
    ///
    /// Mutation: in `Close::drop`, skip the wait on `entered` → `run` returns
    /// while a helper is still inside and the count read right after is short
    /// → fails (intermittently here; the loom model deterministically).
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

    /// A call from inside a job runs alone rather than waiting on itself.
    ///
    /// Mutation: make the busy branch wait for `busy` instead of running the
    /// job → the inner call spins forever → the test hangs.
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

    /// A panicking job still closes the gate, so the next call can run.
    ///
    /// Mutation: release `busy` at the end of `run`'s body instead of in
    /// `Close::drop` → the unwind skips it → the second `run` is refused
    /// (returns false) → fails.
    #[test]
    fn a_panicking_job_closes_the_gate() {
        let gate = JobGate::new(2);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            gate.run(&|_| panic!("boom"), || {});
        }));
        assert!(r.is_err());
        assert!(gate.run(&|_| {}, || {}));
    }
}
