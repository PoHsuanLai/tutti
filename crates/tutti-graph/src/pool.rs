//! [`WorkerPool`]: the threads the parallel graph executor runs a block on
//! (doc 013 §4 and "The parallel executor").
//!
//! The graph defines what it needs of a pool (`tutti_graph::Pool`: run one
//! job on the caller and up to `participants - 1` other threads, return when
//! none is inside it); this is the implementation. The handshake
//! that lends each block's borrowed job to the helpers is
//! `tutti_rt::JobGate`, loom-checked there; this module adds only the
//! threads and how they wait.
//!
//! The pool runs one job at a time. A `run` that finds it busy (another
//! caller's job, or a call from inside a job) runs its job alone on the
//! caller; a `try_run` runs nothing and returns `false`, for a caller with a
//! fallback of its own (a CLAP plugin's `clap.thread-pool` request).
//!
//! # How a helper waits
//!
//! Between jobs a helper **spins** for a short while (a block's job usually
//! arrives within a device period, and a spinning helper joins it in
//! nanoseconds), then **yields** its core for a while longer, then **parks**
//! (crill's progressive backoff, doc 013 §4). A parked helper is woken by
//! the caller when a job opens — `Thread::unpark`, the one system call the
//! audio thread makes for the pool — and only as many as the job can keep
//! busy (`wake`, the plan's widest level): Ardour's bounded wakeups.
//!
//! The caller **never waits for a helper to wake**: it starts on the job at
//! once, and a helper that is still waking when the job's tasks run out
//! never enters it. It waits only for helpers already inside the job, and a
//! helper inside a job never yields or parks there: it takes tasks, spins
//! for [`WorkerPoolBuilder::helper_leave_after`] (15 µs by default) when
//! none is ready, and then leaves the job (`tutti_rt::TaskGraph`'s
//! "Waiting"). Yielding and parking happen only between jobs, outside the
//! gate.
//!
//! The caller's own wait, for a helper still running a task or still inside
//! the gate, spins, and on Linux yields its core once per
//! [`WorkerPoolBuilder::caller_yield_after`] (250 µs by default) in which
//! the helper made no progress (`tutti-rt`'s `src/spin.rs`). That yield is
//! for a helper queued behind the caller on the caller's core, and it only
//! helps one at the caller's priority (below).
//!
//! The pool's four waits are its builder's: [`spin_budget`](WorkerPoolBuilder::spin_budget)
//! and [`idle`](WorkerPoolBuilder::idle) between jobs, and the two inside a
//! job above, which the executor reads through `Pool::waits` and the gate
//! keeps for its close (`tutti_rt::BlockWaits`).
//!
//! # Priority
//!
//! A helper runs graph nodes on the audio thread's behalf, so a host that
//! promotes its audio callback (`SCHED_FIFO`, MMCSS, a macOS audio
//! workgroup) must promote the helpers the same way, **at the callback's
//! priority**: give [`WorkerPoolBuilder::start_handler`] a hook, which each
//! helper runs once on its own thread before it takes any work. The engine
//! does not do this itself, for the same reason it does not promote the
//! callback: the device layer owns the policy (and on Linux the privilege).
//! `tutti-session`'s `SessionBuilder::render_worker_start` passes a hook to
//! the live pool.
//!
//! If they run below it (the default when the hook is not given: `SCHED_OTHER`
//! helpers under a `SCHED_FIFO` callback), a helper that holds a task and
//! shares the callback's core cannot run while the callback waits for it:
//! the callback's yield moves it behind threads of its own priority only.
//! The block then lasts until the kernel moves the helper to another core,
//! or until RT throttling takes the core from the callback (950 ms in every
//! second by default on Linux): a dropout. At the callback's priority, the
//! yield lets the helper finish its task (`tutti-rt`'s
//! `tests/spin_starvation.rs` measures both on Linux; on Windows and macOS,
//! where the caller does not yield, the scheduler round-robins
//! equal-priority real-time threads at a quantum's end). Above it is wrong
//! too: a helper would then preempt the callback on its core.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::{BlockWaits, Pool};
use tutti_rt::{Doorbell, JobGate};

type StartHandler = Arc<dyn Fn(usize) + Send + Sync>;
type ThreadName = Box<dyn FnMut(usize) -> String>;

struct Shared {
    gate: JobGate,
    stop: AtomicBool,
    /// Helper `i`'s doorbell is `bells[i - 1]`.
    bells: Box<[Doorbell]>,
    spin: Duration,
    idle: Duration,
}

impl Shared {
    /// Wake up to `n` sleeping helpers. Runs on the caller right after the
    /// gate opens (the write each helper's `pending` check reads).
    fn wake(&self, n: usize) {
        Doorbell::ring_first(&self.bells, n);
    }

    /// Helper `i`'s loop.
    fn helper(&self, i: usize) {
        let me = &self.bells[i - 1];
        let mut served = 0u64;
        let mut idle_since = Instant::now();
        while !self.stop.load(Ordering::Acquire) {
            if self.gate.help(&mut served) {
                idle_since = Instant::now();
                continue;
            }
            let idle = idle_since.elapsed();
            if idle < self.spin {
                std::hint::spin_loop();
            } else if idle < self.spin + self.idle {
                std::thread::yield_now();
            } else {
                // A job opened (or the pool stopped) before the check is
                // seen; one after rings this bell.
                me.wait(|| self.gate.pending(served) || self.stop.load(Ordering::Acquire));
                idle_since = Instant::now();
            }
        }
    }
}

/// A pool of helper threads for the parallel graph executor. See the module
/// docs (`src/pool.rs`).
///
/// ```
/// use std::sync::Arc;
/// use tutti_graph::WorkerPool;
///
/// # fn main() -> std::io::Result<()> {
/// // The caller and three helpers.
/// let pool = Arc::new(WorkerPool::builder().num_threads(4).build()?);
/// # let (_editor, mut executor) = tutti_graph::Editor::new(tutti_graph::Prepare::new(
/// #     tutti_types::SampleRate(48_000.0),
/// #     tutti_types::MaxBlock::of(tutti_types::Samples(256)),
/// # ));
/// executor.set_pool(Some(pool)); // before the executor goes to the audio thread
/// # Ok(())
/// # }
/// ```
pub struct WorkerPool {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

/// Configures a [`WorkerPool`].
pub struct WorkerPoolBuilder {
    num_threads: usize,
    spin: Duration,
    idle: Duration,
    waits: BlockWaits,
    start_handler: Option<StartHandler>,
    thread_name: ThreadName,
}

impl std::fmt::Debug for WorkerPoolBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerPoolBuilder")
            .field("num_threads", &self.num_threads)
            .field("spin", &self.spin)
            .field("idle", &self.idle)
            .field("waits", &self.waits)
            .finish_non_exhaustive()
    }
}

impl WorkerPoolBuilder {
    /// The threads that run a job: the caller and `n - 1` helpers. `1` is a
    /// pool of no helpers (every job runs on the caller). Zero, the
    /// default, is one per available core (`available_parallelism`).
    #[must_use]
    pub fn num_threads(mut self, n: usize) -> Self {
        self.num_threads = n;
        self
    }

    /// Between jobs, how long a helper spins after its last job before it
    /// starts yielding. Default 50 µs.
    #[must_use]
    pub fn spin_budget(mut self, d: Duration) -> Self {
        self.spin = d;
        self
    }

    /// Between jobs, how long a helper then yields its core before it
    /// parks. Default 500 µs.
    #[must_use]
    pub fn idle(mut self, d: Duration) -> Self {
        self.idle = d;
        self
    }

    /// Inside a job, how long the caller (the audio thread) waits on a
    /// helper with no task finishing before it yields its core once, on
    /// Linux and Android; elsewhere it only spins. Default 250 µs: longer
    /// than a healthy block goes without a task finishing, and short enough
    /// that a helper starved behind a `SCHED_FIFO` caller on its core gets
    /// it back with most of a 64-frame block left
    /// ([`BlockWaits::caller_yield_after`]). Zero yields after about 128
    /// waits without progress, the granularity of the clock it is timed by.
    #[must_use]
    pub fn caller_yield_after(mut self, d: Duration) -> Self {
        self.waits.caller_yield_after = d;
        self
    }

    /// Inside a job, how long a helper with no task to take spins before it
    /// leaves the job to the others. Default 15 µs: enough to pick up the
    /// successors of a task another participant is finishing
    /// ([`BlockWaits::helper_leave_after`]). Zero leaves after about 128
    /// idle waits, the granularity of the clock it is timed by.
    #[must_use]
    pub fn helper_leave_after(mut self, d: Duration) -> Self {
        self.waits.helper_leave_after = d;
        self
    }

    /// A hook each helper runs once on its own thread before taking work,
    /// with the helper's index (`1..num_threads`; the caller is 0) — where a
    /// host raises the thread's priority to the audio callback's or joins its
    /// audio workgroup. Under a real-time callback the helpers must run at
    /// its priority (see the module docs, "Priority").
    ///
    /// A hook that panics ends only its own helper: `build` still returns
    /// `Ok`, and jobs run on the remaining threads. A hook that blocks keeps
    /// its helper from ever taking work and makes dropping the pool wait for
    /// it; `build` does not wait for hooks.
    #[must_use]
    pub fn start_handler(mut self, hook: impl Fn(usize) + Send + Sync + 'static) -> Self {
        self.start_handler = Some(Arc::new(hook));
        self
    }

    /// The name of helper `index` (`1..num_threads`). Default
    /// `"tutti-worker-<index>"`.
    #[must_use]
    pub fn thread_name(mut self, name: impl FnMut(usize) -> String + 'static) -> Self {
        self.thread_name = Box::new(name);
        self
    }

    /// Start the helpers. Control thread; spawns threads.
    ///
    /// # Errors
    ///
    /// If a thread cannot be spawned. The helpers already started are
    /// stopped and joined first.
    pub fn build(mut self) -> io::Result<WorkerPool> {
        let participants = match self.num_threads {
            0 => std::thread::available_parallelism().map_or(1, |n| n.get()),
            n => n,
        };
        let helpers = participants - 1;
        let shared = Arc::new(Shared {
            gate: JobGate::with_waits(participants, self.waits),
            stop: AtomicBool::new(false),
            bells: (0..helpers).map(|_| Doorbell::new()).collect(),
            spin: self.spin,
            idle: self.idle,
        });
        // Dropped on an early return: stops and joins the helpers running.
        let mut pool = WorkerPool {
            shared,
            threads: Vec::with_capacity(helpers),
        };
        for i in 1..=helpers {
            let shared = Arc::clone(&pool.shared);
            let hook = self.start_handler.clone();
            let thread = std::thread::Builder::new()
                .name((self.thread_name)(i))
                .spawn(move || {
                    if let Some(hook) = hook {
                        hook(i);
                    }
                    shared.helper(i);
                })?;
            pool.threads.push(thread);
        }
        Ok(pool)
    }
}

impl WorkerPool {
    /// Configure a pool: one thread per available core, the caller included
    /// (see [`num_threads`](WorkerPoolBuilder::num_threads)), with the
    /// default waits.
    pub fn builder() -> WorkerPoolBuilder {
        WorkerPoolBuilder {
            num_threads: 0,
            spin: Duration::from_micros(50),
            idle: Duration::from_micros(500),
            waits: BlockWaits::DEFAULT,
            start_handler: None,
            thread_name: Box::new(|i| format!("tutti-worker-{i}")),
        }
    }
}

impl Pool for WorkerPool {
    fn participants(&self) -> usize {
        self.shared.gate.participants()
    }

    fn run(&self, wake: usize, job: &(dyn Fn(usize) + Sync)) {
        let shared = &*self.shared;
        shared.gate.run(job, || shared.wake(wake.saturating_sub(1)));
    }

    /// `false` when the gate is busy: another caller's job is open, or this
    /// call is made from inside one.
    fn try_run(&self, wake: usize, job: &(dyn Fn(usize) + Sync)) -> bool {
        let shared = &*self.shared;
        shared
            .gate
            .try_run(job, || shared.wake(wake.saturating_sub(1)))
    }

    /// The builder's [`caller_yield_after`](WorkerPoolBuilder::caller_yield_after)
    /// and [`helper_leave_after`](WorkerPoolBuilder::helper_leave_after),
    /// as the gate holds them.
    fn waits(&self) -> BlockWaits {
        self.shared.gate.waits()
    }
}

impl std::fmt::Debug for WorkerPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerPool")
            .field("participants", &self.participants())
            .finish()
    }
}

impl Drop for WorkerPool {
    /// Stops and joins the helpers. Control thread (it joins threads).
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        // A helper's `wait` re-reads `stop` after announcing it sleeps: it
        // sees this store or this ring finds it asleep. One that has not
        // started yet sees `stop` at its loop's top.
        Doorbell::ring_first(&self.shared.bells, self.shared.bells.len());
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    /// Helpers take part in jobs — also after they have parked — with
    /// distinct indices, and a job never outlives `run`.
    #[test]
    fn helpers_join_jobs_before_and_after_parking() {
        let pool = WorkerPool::builder()
            .num_threads(3)
            .spin_budget(Duration::from_micros(10))
            .idle(Duration::from_micros(10))
            .build()
            .expect("a worker pool");
        // Until both helpers have joined a job, or a deadline. The deadline
        // is for a loaded CI runner, where nextest's other tests can keep a
        // runnable helper off a core for tens of milliseconds; a fixed count
        // of 200 jobs (about 40 ms of the caller's spinning) failed there.
        let seen = |pool: &WorkerPool| {
            let mask = AtomicUsize::new(0);
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                pool.run(3, &|i| {
                    mask.fetch_or(1 << i, Ordering::Relaxed);
                    // Keep participant 0 busy long enough for a helper to
                    // wake and join.
                    if i == 0 {
                        let t = Instant::now();
                        while t.elapsed() < Duration::from_micros(200) {
                            std::hint::spin_loop();
                        }
                    }
                });
                if mask.load(Ordering::Relaxed) == 0b111 {
                    break;
                }
            }
            mask.load(Ordering::Relaxed)
        };
        assert_eq!(seen(&pool), 0b111, "both helpers joined while spinning");
        // Wait for both to park (20 µs of backoff; the deadline is for a
        // loaded CI runner, not the expected time).
        let parked = |pool: &WorkerPool| pool.shared.bells.iter().all(|b| b.is_asleep());
        let deadline = Instant::now() + Duration::from_secs(10);
        while !parked(&pool) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(parked(&pool), "both helpers parked");
        assert_eq!(seen(&pool), 0b111, "both helpers woke and joined");
    }

    /// The start hook runs once on each helper, on its own thread.
    #[test]
    fn the_start_hook_runs_on_each_helper() {
        let started = Arc::new(AtomicUsize::new(0));
        let s = Arc::clone(&started);
        let pool = WorkerPool::builder()
            .num_threads(4)
            .start_handler(move |i| {
                assert!(
                    std::thread::current()
                        .name()
                        .is_some_and(|n| n == format!("tutti-worker-{i}"))
                );
                s.fetch_add(1, Ordering::Relaxed);
            })
            .build()
            .expect("a worker pool");
        let t = Instant::now();
        while started.load(Ordering::Relaxed) < 3 && t.elapsed() < Duration::from_secs(5) {
            std::thread::yield_now();
        }
        assert_eq!(started.load(Ordering::Relaxed), 3);
        drop(pool);
    }

    /// `try_run` from inside a job of the pool runs nothing and says so; on
    /// an idle pool it runs the job.
    #[test]
    fn try_run_inside_a_job_refuses() {
        let pool = WorkerPool::builder()
            .num_threads(2)
            .build()
            .expect("a worker pool");
        let inner = AtomicUsize::new(0);
        let nested = AtomicUsize::new(0);
        assert!(pool.try_run(1, &|i| {
            if i == 0 {
                let accepted = pool.try_run(1, &|_| {
                    inner.fetch_add(1, Ordering::Relaxed);
                });
                nested.store(1 + accepted as usize, Ordering::Relaxed);
            }
        }));
        assert_eq!(nested.load(Ordering::Relaxed), 1, "the nested call refused");
        assert_eq!(inner.load(Ordering::Relaxed), 0);
    }

    /// The builder's in-job waits are what the pool reports to the executor
    /// and what its gate's close waits by; a pool built without them reports
    /// the defaults.
    #[test]
    fn the_in_job_waits_are_the_builders() {
        let waits = BlockWaits {
            caller_yield_after: Duration::from_micros(400),
            helper_leave_after: Duration::from_micros(3),
        };
        let pool = WorkerPool::builder()
            .num_threads(2)
            .caller_yield_after(waits.caller_yield_after)
            .helper_leave_after(waits.helper_leave_after)
            .build()
            .expect("a worker pool");
        assert_eq!(pool.waits(), waits);
        assert_eq!(pool.shared.gate.waits(), waits);
        let plain = WorkerPool::builder()
            .num_threads(2)
            .build()
            .expect("a worker pool");
        assert_eq!(plain.waits(), BlockWaits::DEFAULT);
    }

    /// A one-participant pool runs every job on the caller.
    #[test]
    fn a_pool_of_one_runs_on_the_caller() {
        let pool = WorkerPool::builder()
            .num_threads(1)
            .build()
            .expect("a worker pool");
        let n = AtomicUsize::new(0);
        pool.run(4, &|i| {
            assert_eq!(i, 0);
            n.fetch_add(1, Ordering::Relaxed);
        });
        assert_eq!(n.load(Ordering::Relaxed), 1);
    }

    /// Helpers carry the builder's names.
    #[test]
    fn helpers_are_named_by_the_builder() {
        let names = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::clone(&names);
        let pool = WorkerPool::builder()
            .num_threads(3)
            .thread_name(|i| format!("custom-{i}"))
            .start_handler(move |_| {
                let name = std::thread::current().name().map(str::to_owned);
                seen.lock().unwrap().push(name.unwrap());
            })
            .build()
            .expect("a worker pool");
        let t = Instant::now();
        while names.lock().unwrap().len() < 2 && t.elapsed() < Duration::from_secs(5) {
            std::thread::yield_now();
        }
        drop(pool);
        let mut names = names.lock().unwrap().clone();
        names.sort();
        assert_eq!(names, ["custom-1", "custom-2"]);
    }

    /// A helper whose start handler panics ends alone: the pool is built, a
    /// job closes without waiting for it, and the other helper still joins
    /// (as participant 1: the gate numbers participants by arrival).
    #[test]
    fn a_panicking_start_handler_leaves_a_working_pool() {
        let pool = WorkerPool::builder()
            .num_threads(3)
            .start_handler(|i| {
                if i == 1 {
                    panic!("start handler of helper 1");
                }
            })
            .spin_budget(Duration::from_micros(10))
            .idle(Duration::from_micros(10))
            .build()
            .expect("a worker pool");
        let mask = AtomicUsize::new(0);
        let deadline = Instant::now() + Duration::from_secs(10);
        // The gate hands out participant indices in arrival order, so the one
        // live helper is participant 1.
        while mask.load(Ordering::Relaxed) & 0b010 == 0 && Instant::now() < deadline {
            pool.run(3, &|i| {
                mask.fetch_or(1 << i, Ordering::Relaxed);
                if i == 0 {
                    let t = Instant::now();
                    while t.elapsed() < Duration::from_micros(200) {
                        std::hint::spin_loop();
                    }
                }
            });
        }
        let mask = mask.load(Ordering::Relaxed);
        assert_eq!(mask, 0b011, "the caller and the one live helper ran");
    }

    /// Zero threads, the default, is one per available core.
    #[test]
    fn the_default_is_one_thread_per_core() {
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        let pool = WorkerPool::builder().build().expect("a worker pool");
        assert_eq!(pool.participants(), cores);
    }
}
