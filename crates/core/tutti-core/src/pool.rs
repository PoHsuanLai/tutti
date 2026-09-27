//! [`WorkerPool`]: the threads the parallel graph executor runs a block on
//! (doc 013 §4 and Phase 6).
//!
//! The graph defines what it needs of a pool (`tutti_graph::Pool`: run one
//! job on the caller and up to `participants - 1` other threads, return when
//! none is inside it); this is the engine's implementation. The handshake
//! that lends each block's borrowed job to the helpers is
//! `tutti_types::JobGate`, loom-checked there; this module adds only the
//! threads and how they wait.
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
//! never enters it. It waits only for helpers already inside the job, which
//! are finishing tasks.
//!
//! # Priority
//!
//! A helper runs graph nodes on the audio thread's behalf, so a host that
//! promotes its audio callback (`SCHED_FIFO`, MMCSS, a macOS audio
//! workgroup) should promote the helpers the same way: give
//! [`WorkerPoolBuilder::on_start`] a hook, which each helper runs once on its
//! own thread before it takes any work. The engine does not do this itself,
//! for the same reason it does not promote the callback: the device layer
//! owns the policy (and on Linux the privilege).

use std::sync::atomic::{fence, AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::{JoinHandle, Thread};
use std::time::{Duration, Instant};

use tutti_graph::Pool;
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
    on_start: Option<OnStart>,
    name: String,
}

impl WorkerPoolBuilder {
    /// How long a helper spins after its last job before yielding. Default
    /// 50 µs.
    pub fn spin(mut self, d: Duration) -> Self {
        self.spin = d;
        self
    }

    /// How long it then yields its core before parking. Default 500 µs.
    pub fn idle(mut self, d: Duration) -> Self {
        self.idle = d;
        self
    }

    /// A hook each helper runs once on its own thread before taking work,
    /// with its index — where a host raises the thread's priority or joins
    /// an audio workgroup (see the module docs).
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
            gate: JobGate::new(self.participants),
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
        let threads = (1..=helpers)
            .map(|i| {
                let shared = Arc::clone(&shared);
                let hook = self.on_start.clone();
                std::thread::Builder::new()
                    .name(format!("{}-{i}", self.name))
                    .spawn(move || {
                        let _ = shared.sleepers[i - 1].thread.set(std::thread::current());
                        if let Some(hook) = hook {
                            hook(i);
                        }
                        shared.helper(i);
                    })
                    .expect("spawn a worker thread")
            })
            .collect();
        WorkerPool { shared, threads }
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
        for s in self.shared.sleepers.iter() {
            if let Some(t) = s.thread.get() {
                t.unpark();
            }
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
    /// count stays 1 → fails.
    #[test]
    fn helpers_join_jobs_before_and_after_parking() {
        let pool = WorkerPool::builder(3)
            .spin(Duration::from_micros(10))
            .idle(Duration::from_micros(10))
            .build();
        let seen = |pool: &WorkerPool| {
            let mask = AtomicUsize::new(0);
            for _ in 0..200 {
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
        // Long enough for both to park.
        std::thread::sleep(Duration::from_millis(50));
        assert!(pool
            .shared
            .sleepers
            .iter()
            .all(|s| s.asleep.load(Ordering::Relaxed)));
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
