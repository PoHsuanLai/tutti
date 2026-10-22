//! [`WorkerPool`]: the threads the parallel graph executor runs a block on
//! (doc 013 §4 and "The parallel executor").
//!
//! The graph defines what it needs of a pool (`tutti_graph::Pool`: run one
//! job on the caller and up to `participants - 1` other threads, return when
//! none is inside it); this is the engine's implementation. The handshake
//! that lends each block's borrowed job to the helpers is
//! `tutti_types::JobGate`, loom-checked there; this module adds only the
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
//! none is ready, and then leaves the job (`tutti_types::TaskGraph`'s
//! "Waiting"). Yielding and parking happen only between jobs, outside the
//! gate.
//!
//! The caller's own wait, for a helper still running a task or still inside
//! the gate, spins, and on Linux yields its core once per
//! [`WorkerPoolBuilder::caller_yield_after`] (250 µs by default) in which
//! the helper made no progress (`tutti_types`' `rt/spin.rs`). That yield is
//! for a helper queued behind the caller on the caller's core, and it only
//! helps one at the caller's priority (below).
//!
//! The pool's four waits are its builder's: [`spin`](WorkerPoolBuilder::spin)
//! and [`idle`](WorkerPoolBuilder::idle) between jobs, and the two inside a
//! job above, which the executor reads through `Pool::waits` and the gate
//! keeps for its close (`tutti_types::BlockWaits`).
//!
//! # Priority
//!
//! A helper runs graph nodes on the audio thread's behalf, so a host that
//! promotes its audio callback (`SCHED_FIFO`, MMCSS, a macOS audio
//! workgroup) must promote the helpers the same way, **at the callback's
//! priority**: give [`WorkerPoolBuilder::on_start`] a hook, which each
//! helper runs once on its own thread before it takes any work. The engine
//! does not do this itself, for the same reason it does not promote the
//! callback: the device layer owns the policy (and on Linux the privilege).
//!
//! If they run below it (the default when the hook is not given: `SCHED_OTHER`
//! helpers under a `SCHED_FIFO` callback), a helper that holds a task and
//! shares the callback's core cannot run while the callback waits for it:
//! the callback's yield moves it behind threads of its own priority only.
//! The block then lasts until the kernel moves the helper to another core,
//! or until RT throttling takes the core from the callback (950 ms in every
//! second by default on Linux): a dropout. At the callback's priority, the
//! yield lets the helper finish its task (`tutti-types`'
//! `tests/spin_starvation.rs` measures both on Linux; on Windows and macOS,
//! where the caller does not yield, the scheduler round-robins
//! equal-priority real-time threads at a quantum's end). Above it is wrong
//! too: a helper would then preempt the callback on its core.

use std::sync::atomic::{fence, AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::{JoinHandle, Thread};
use std::time::{Duration, Instant};

use tutti_graph::{BlockWaits, Pool};
use tutti_types::JobGate;

/// A hook run once on each helper thread as it starts, with its index
/// (`1..participants`).
pub type OnStart = Arc<dyn Fn(usize) + Send + Sync>;

/// One helper's sleep flag, on a cache-line pair of its own.
#[repr(align(128))]
struct Sleeper {
    asleep: AtomicBool,
    thread: OnceLock<Thread>,
}

struct Shared {
    gate: JobGate,
    stop: AtomicBool,
    sleepers: Box<[Sleeper]>,
    spin: Duration,
    idle: Duration,
}

impl Shared {
    /// Wake up to `n` sleeping helpers. Runs on the caller right after the
    /// gate opens.
    fn wake(&self, n: usize) {
        if n == 0 {
            return;
        }
        // The other half of the helper's store-load pair (`helper`): either
        // this sees its `asleep`, or it sees the gate open and does not park.
        fence(Ordering::SeqCst);
        let mut woken = 0;
        for s in self.sleepers.iter() {
            if woken == n {
                break;
            }
            if s.asleep.load(Ordering::Relaxed) && s.asleep.swap(false, Ordering::Relaxed) {
                if let Some(t) = s.thread.get() {
                    t.unpark();
                }
                woken += 1;
            }
        }
    }

    /// Helper `i`'s loop.
    fn helper(&self, i: usize) {
        let me = &self.sleepers[i - 1];
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
                me.asleep.store(true, Ordering::Relaxed);
                // Pairs with the fence in `wake`: a job opened before this
                // is seen below; one opened after finds `asleep` set and
                // unparks (an unpark before the park makes it return).
                fence(Ordering::SeqCst);
                if !self.gate.pending(served) && !self.stop.load(Ordering::Acquire) {
                    std::thread::park();
                }
                me.asleep.store(false, Ordering::Relaxed);
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
/// use tutti_core::WorkerPool;
///
/// let pool = Arc::new(WorkerPool::new(4)); // the caller and three helpers
/// # let (_editor, mut executor) = tutti_graph::Editor::new(tutti_graph::Prepare::new(
/// #     tutti_core::SampleRate(48_000.0),
/// #     tutti_core::Samples(256),
/// # ));
/// executor.set_pool(Some(pool)); // before the executor goes to the audio thread
/// ```
pub struct WorkerPool {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

/// Configures a [`WorkerPool`].
pub struct WorkerPoolBuilder {
    participants: usize,
    spin: Duration,
    idle: Duration,
    waits: BlockWaits,
    on_start: Option<OnStart>,
    name: String,
}

impl WorkerPoolBuilder {
    /// Between jobs, how long a helper spins after its last job before it
    /// starts yielding. Default 50 µs.
    pub fn spin(mut self, d: Duration) -> Self {
        self.spin = d;
        self
    }

    /// Between jobs, how long a helper then yields its core before it
    /// parks. Default 500 µs.
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
    pub fn caller_yield_after(mut self, d: Duration) -> Self {
        self.waits.caller_yield_after = d;
        self
    }

    /// Inside a job, how long a helper with no task to take spins before it
    /// leaves the job to the others. Default 15 µs: enough to pick up the
    /// successors of a task another participant is finishing
    /// ([`BlockWaits::helper_leave_after`]). Zero leaves after about 128
    /// idle waits, the granularity of the clock it is timed by.
    pub fn helper_leave_after(mut self, d: Duration) -> Self {
        self.waits.helper_leave_after = d;
        self
    }

    /// A hook each helper runs once on its own thread before taking work,
    /// with its index — where a host raises the thread's priority to the
    /// audio callback's or joins its audio workgroup. Under a real-time
    /// callback the helpers must run at its priority (see the module docs,
    /// "Priority").
    pub fn on_start(mut self, hook: OnStart) -> Self {
        self.on_start = Some(hook);
        self
    }

    /// The helpers' thread name prefix (`"<name>-<index>"`). Default
    /// `"tutti-worker"`.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Start the helpers. Control thread; spawns threads.
    ///
    /// # Panics
    ///
    /// If a thread cannot be spawned.
    pub fn build(self) -> WorkerPool {
        let helpers = self.participants - 1;
        let shared = Arc::new(Shared {
            gate: JobGate::with_waits(self.participants, self.waits),
            stop: AtomicBool::new(false),
            sleepers: (0..helpers)
                .map(|_| Sleeper {
                    asleep: AtomicBool::new(false),
                    thread: OnceLock::new(),
                })
                .collect(),
            spin: self.spin,
            idle: self.idle,
        });
        let mut pool = WorkerPool {
            shared,
            threads: Vec::with_capacity(helpers),
        };
        for i in 1..=helpers {
            let shared = Arc::clone(&pool.shared);
            let hook = self.on_start.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("{}-{i}", self.name))
                .spawn(move || {
                    let _ = shared.sleepers[i - 1].thread.set(std::thread::current());
                    if let Some(hook) = hook {
                        hook(i);
                    }
                    shared.helper(i);
                });
            match spawned {
                Ok(t) => pool.threads.push(t),
                Err(e) => {
                    // Stop and join the helpers already running before
                    // panicking, rather than leak them spinning.
                    drop(pool);
                    panic!("spawn a worker thread: {e}");
                }
            }
        }
        pool
    }
}

impl WorkerPool {
    /// A pool running jobs on `participants` threads: the caller and
    /// `participants - 1` helpers, with the default waits. `1` is a pool of
    /// no helpers (every job runs on the caller). Control thread; spawns
    /// threads.
    ///
    /// # Panics
    ///
    /// If `participants` is zero.
    pub fn new(participants: usize) -> Self {
        Self::builder(participants).build()
    }

    /// A pool of one participant per available core (`available_parallelism`),
    /// the caller included.
    pub fn per_core() -> Self {
        Self::new(std::thread::available_parallelism().map_or(1, |n| n.get()))
    }

    /// Configure a pool of `participants` (see [`new`](Self::new)).
    ///
    /// # Panics
    ///
    /// If `participants` is zero.
    pub fn builder(participants: usize) -> WorkerPoolBuilder {
        assert!(participants > 0, "a pool needs its caller");
        WorkerPoolBuilder {
            participants,
            spin: Duration::from_micros(50),
            idle: Duration::from_micros(500),
            waits: BlockWaits::DEFAULT,
            on_start: None,
            name: "tutti-worker".into(),
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
        // Pairs with the helper's fence before it parks: either it sees
        // `stop` and does not park, or its park comes after this and the
        // unpark below makes it return.
        fence(Ordering::SeqCst);
        // Through the join handle, not the sleeper's `OnceLock`: a helper
        // that has not started yet has not set the latter.
        for t in self.threads.iter() {
            t.thread().unpark();
        }
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
    ///
    /// Mutation: make `wake` a no-op → after the helpers park, no job gets a
    /// helper again (participant 0 never waits for one) → the second phase's
    /// mask stays 0b001 until its deadline → fails (run: after 10 s).
    #[test]
    fn helpers_join_jobs_before_and_after_parking() {
        let pool = WorkerPool::builder(3)
            .spin(Duration::from_micros(10))
            .idle(Duration::from_micros(10))
            .build();
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
        let parked = |pool: &WorkerPool| {
            pool.shared
                .sleepers
                .iter()
                .all(|s| s.asleep.load(Ordering::Relaxed))
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !parked(&pool) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(parked(&pool), "both helpers parked");
        assert_eq!(seen(&pool), 0b111, "both helpers woke and joined");
    }

    /// The start hook runs once on each helper, on its own thread.
    ///
    /// Mutation: skip the hook in `build` → the count stays 0 → fails.
    #[test]
    fn the_start_hook_runs_on_each_helper() {
        let started = Arc::new(AtomicUsize::new(0));
        let s = Arc::clone(&started);
        let pool = WorkerPool::builder(4)
            .on_start(Arc::new(move |i| {
                assert!(std::thread::current()
                    .name()
                    .is_some_and(|n| n == format!("tutti-worker-{i}")));
                s.fetch_add(1, Ordering::Relaxed);
            }))
            .build();
        let t = Instant::now();
        while started.load(Ordering::Relaxed) < 3 && t.elapsed() < Duration::from_secs(5) {
            std::thread::yield_now();
        }
        assert_eq!(started.load(Ordering::Relaxed), 3);
        drop(pool);
    }

    /// `try_run` from inside a job of the pool runs nothing and says so; on
    /// an idle pool it runs the job.
    ///
    /// Mutation: have `WorkerPool::try_run` call `run` and return `true` →
    /// the nested call runs and returns `true` → fails.
    #[test]
    fn try_run_inside_a_job_refuses() {
        let pool = WorkerPool::new(2);
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
    ///
    /// Mutations (run): drop `caller_yield_after`'s store → fails; build the
    /// gate with `JobGate::new` (the defaults) → fails.
    #[test]
    fn the_in_job_waits_are_the_builders() {
        let waits = BlockWaits {
            caller_yield_after: Duration::from_micros(400),
            helper_leave_after: Duration::from_micros(3),
        };
        let pool = WorkerPool::builder(2)
            .caller_yield_after(waits.caller_yield_after)
            .helper_leave_after(waits.helper_leave_after)
            .build();
        assert_eq!(pool.waits(), waits);
        assert_eq!(pool.shared.gate.waits(), waits);
        assert_eq!(WorkerPool::new(2).waits(), BlockWaits::DEFAULT);
    }

    /// A one-participant pool runs every job on the caller.
    ///
    /// Mutation: have `run` skip the job when there are no helpers → the
    /// count stays 0 → fails.
    #[test]
    fn a_pool_of_one_runs_on_the_caller() {
        let pool = WorkerPool::new(1);
        let n = AtomicUsize::new(0);
        pool.run(4, &|i| {
            assert_eq!(i, 0);
            n.fetch_add(1, Ordering::Relaxed);
        });
        assert_eq!(n.load(Ordering::Relaxed), 1);
    }
}
