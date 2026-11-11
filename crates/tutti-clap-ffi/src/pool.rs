//! Serving `clap.thread-pool`: a plugin's `request_exec(host, n)` from inside
//! its `process` runs `exec(plugin, 0..n)` on the [`TaskPool`] the plugin was
//! given, and returns once every task has run.
//!
//! # Which calls are served
//!
//! `ext/thread-pool.h` allows the call only from the audio thread, inside
//! `process`, and never from the pool; the host "should check that the
//! plugin is within the process call, and if not, reject". The check is a
//! thread-local: `process` serves a [`PoolRequest`] on its own thread for
//! exactly the span of the plugin's `process` call, and `request_exec` serves
//! a call only on the thread serving one, for the same instance. Every other
//! call (outside `process`, on another thread, from another instance, or
//! from inside a task, since the request is taken while it runs) is refused
//! with `false`, and the plugin runs its tasks itself, as the header tells it
//! to.
//!
//! # Running the tasks
//!
//! The pool runs one job; each participant claims task indices from a shared
//! counter until none is left, so every index runs exactly once whatever
//! the number of participants, one included. [`TaskPool::try_run`] returns
//! only once no participant is inside the job, so `request_exec` returns
//! after the last task, and every task's writes happen-before the plugin
//! reads them (the pool's contract).
//!
//! The pool may be busy: the caller can itself be a participant of a graph
//! block running on the same pool. The request is then refused
//! (`try_run` returns `false`), as with no pool: the plugin's own fallback is
//! never worse than running the tasks alone on the caller, and the request
//! never waits on the job it was made from.
//!
//! Nothing here allocates, locks or frees: the request lives on `process`'s
//! stack, the counter on `request_exec`'s.

use std::cell::Cell;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use clap_sys::plugin::clap_plugin;

/// A pool of threads a job can be spread over.
///
/// # Safety
///
/// The implementor owes the contract below, which this crate cannot check.
/// `job` borrows the caller's stack (the task counter, the plugin), so when
/// `try_run` returns `true`:
///
/// - no participant is inside `job`, and none can enter it later (a slow,
///   leaked or late helper would run the plugin's tasks on a dead frame);
/// - every participant ran `job` with its own `i`, distinct from the others';
/// - every task ran: `true` means the job was run to completion by the
///   participants that took part (the caller included), not skipped.
///
/// When the pool is busy it runs nothing and returns `false` rather than
/// wait. A `try_run` that returns early with `true` is a use-after-free of
/// `request_exec`'s stack.
///
/// Safe code cannot claim it:
///
/// ```compile_fail
/// struct Early;
///
/// impl tutti_clap_ffi::TaskPool for Early {
///     fn participants(&self) -> usize {
///         2
///     }
///
///     fn try_run(&self, _wake: usize, _job: &(dyn Fn(usize) + Sync)) -> bool {
///         true // "every participant has left" -- nobody ran anything
///     }
/// }
/// ```
pub unsafe trait TaskPool: Send + Sync + 'static {
    /// Participants a job can have, the caller included. At least 1.
    fn participants(&self) -> usize;

    /// Run `job` on up to `wake` participants (the caller included) and
    /// return `true` once all have left it, or, when the pool is busy, run
    /// nothing and return `false`. Every participant runs `job` with its own
    /// index; the participants' writes happen-before this call returns.
    ///
    /// A participant runs the job under the audio thread's float mode: an
    /// implementation that moves it to a helper thread sets that thread's
    /// denormal mode first.
    fn try_run(&self, wake: usize, job: &(dyn Fn(usize) + Sync)) -> bool;
}

/// A [`tutti_graph::WorkerPool`] as a [`TaskPool`]: every participant runs
/// under the audio thread's float mode, as the helpers run the plugin's DSP on
/// its behalf.
///
/// It takes the one pool whose `try_run` is known to keep the contract (its
/// gate closes a job to late entrants and waits for the ones inside), not any
/// `tutti_graph::Pool`: that trait is safe to implement, and a wrong
/// implementation would make the `unsafe impl` below false.
pub struct GraphPool(Arc<dyn tutti_graph::Pool>);

impl GraphPool {
    /// `pool`, served to a plugin's `request_exec`.
    ///
    /// Only a `WorkerPool` is accepted. Another pool is refused at compile
    /// time, since a safe `tutti_graph::Pool` that returned `true` early would
    /// break the `unsafe impl` below:
    ///
    /// ```compile_fail
    /// use std::sync::Arc;
    ///
    /// struct Early;
    ///
    /// impl tutti_graph::Pool for Early {
    ///     fn participants(&self) -> usize {
    ///         2
    ///     }
    ///     fn run(&self, _wake: usize, _job: &(dyn Fn(usize) + Sync)) {}
    ///     fn try_run(&self, _wake: usize, _job: &(dyn Fn(usize) + Sync)) -> bool {
    ///         true
    ///     }
    ///     fn waits(&self) -> tutti_rt::BlockWaits {
    ///         unimplemented!()
    ///     }
    /// }
    ///
    /// let _ = tutti_clap_ffi::GraphPool::new(Arc::new(Early));
    /// ```
    #[must_use]
    pub fn new(pool: Arc<tutti_graph::WorkerPool>) -> Self {
        Self(pool)
    }

    /// Any [`tutti_graph::Pool`], for a test that wraps the worker pool (to
    /// gate allocation on its participants, say).
    ///
    /// # Safety
    /// `pool.try_run` must keep [`TaskPool::try_run`]'s contract: when it
    /// returns `true`, no participant is inside the job or can enter it later,
    /// and every participant ran it. A wrapper that only decorates the job of
    /// a `WorkerPool` does.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    #[must_use]
    pub unsafe fn from_pool_for_test(pool: Arc<dyn tutti_graph::Pool>) -> Self {
        Self(pool)
    }
}

impl std::fmt::Debug for GraphPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphPool")
            .field("participants", &self.0.participants())
            .finish()
    }
}

// SAFETY: `GraphPool::new` takes a `WorkerPool`, whose `try_run` runs the job
// through its gate: a job is open to participants only while the gate is held,
// the caller waits for every participant to leave before it returns `true`, and
// a busy gate returns `false` having run nothing. The wrapper only adds
// `with_no_denormals` inside the job, which returns before the participant
// leaves it. (`from_pool_for_test` puts the same duty on its caller.)
unsafe impl TaskPool for GraphPool {
    fn participants(&self) -> usize {
        self.0.participants()
    }

    fn try_run(&self, wake: usize, job: &(dyn Fn(usize) + Sync)) -> bool {
        self.0
            .try_run(wake, &|i| tutti_rt::with_no_denormals(|| job(i)))
    }
}

/// The plugin's `clap_plugin_thread_pool::exec`.
type ExecFn = unsafe extern "C" fn(plugin: *const clap_plugin, task_index: u32);

/// A pool paired with the `exec` entry of the plugin it serves.
///
/// Built by [`Plugin::pool_handle`](crate::Plugin::pool_handle) and handed to
/// [`AudioSide::process`](crate::AudioSide::process), which serves it for the
/// span of one `process` call.
///
/// The handle names the instance whose vtable `exec` came from (a `Weak`
/// of its private core, which also keeps the address from being reused), and
/// `process` serves it only for that instance: a handle given to another
/// instance's `process` is ignored, as if there were no pool, so `exec` is
/// never called with a `clap_plugin` it does not belong to, and never after
/// its library is gone (a live instance's core is only reachable while its
/// `Plugin`, and so its library, is).
#[derive(Clone)]
pub struct PoolHandle {
    pub(crate) pool: Arc<dyn TaskPool>,
    pub(crate) exec: ExecFn,
    pub(crate) owner: std::sync::Weak<crate::plugin::Core>,
}

impl PoolHandle {
    /// Whether this handle was made for the instance `core` belongs to.
    pub(crate) fn is_for(&self, core: &Arc<crate::plugin::Core>) -> bool {
        std::ptr::eq(self.owner.as_ptr(), Arc::as_ptr(core))
    }
}

impl std::fmt::Debug for PoolHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PoolHandle")
            .field("participants", &self.pool.participants())
            .finish_non_exhaustive()
    }
}

/// What one `process` call lends its `request_exec` calls: the instance's
/// pool, plugin and `exec`.
pub(crate) struct PoolRequest<'a> {
    /// The instance's host, which `request_exec` matches against the
    /// `host_data` it is called with.
    host: usize,
    pool: &'a dyn TaskPool,
    plugin: *const clap_plugin,
    exec: ExecFn,
}

thread_local! {
    /// The request this thread's current `process` call serves, or null.
    /// `const`-initialised with no destructor, so reading it allocates
    /// nothing.
    static CURRENT: Cell<*const PoolRequest<'static>> = const { Cell::new(ptr::null()) };
    /// Whether this thread is running a served task.
    static IN_TASK: Cell<bool> = const { Cell::new(false) };
}

/// Whether the calling thread is running a task of a served `clap.thread-pool`
/// request: the audio thread waits for it, so it is held to the audio
/// thread's rules. Allocation-free.
pub(crate) fn in_served_task() -> bool {
    IN_TASK.with(Cell::get)
}

impl<'a> PoolRequest<'a> {
    /// A request for `plugin`'s `exec` on `handle`'s pool, for the instance
    /// whose host is `host`. `plugin` must stay valid while the request is
    /// served; the caller (`process`) holds the plugin's audio claim for it.
    pub(crate) fn new(host: usize, handle: &'a PoolHandle, plugin: *const clap_plugin) -> Self {
        Self {
            host,
            pool: &*handle.pool,
            plugin,
            exec: handle.exec,
        }
    }

    /// Run `process` (the plugin's `process` call) with this as the request
    /// `request_exec` serves on this thread, and stop serving it when
    /// `process` returns or unwinds.
    pub(crate) fn serve<R>(&self, process: impl FnOnce() -> R) -> R {
        let _served = Swap::new(ptr::from_ref(self).cast::<PoolRequest<'static>>());
        process()
    }

    /// Run `exec(plugin, 0..num_tasks)` on the pool and return when all
    /// have run, or return `false` at once, having run none, when the pool
    /// is busy.
    fn run(&self, num_tasks: u32) -> bool {
        if num_tasks == 0 {
            return true;
        }
        let tasks = Tasks {
            next: AtomicU64::new(0),
            num_tasks: u64::from(num_tasks),
            plugin: self.plugin,
            exec: self.exec,
        };
        let wake = (num_tasks as usize).min(self.pool.participants());
        let ran = self.pool.try_run(wake, &|_| {
            let _task = InTask::enter();
            tasks.drain();
        });
        // `true` is the pool's word that every task ran (`TaskPool`'s contract):
        // every index has been claimed, and the participants that claimed them
        // are out of the job.
        debug_assert!(
            !ran || tasks.next.load(Ordering::Relaxed) >= tasks.num_tasks,
            "a pool answered `true` without running every task"
        );
        ran
    }
}

/// Marks this thread as running a served task until dropped.
struct InTask(bool);

impl InTask {
    fn enter() -> Self {
        Self(IN_TASK.with(|t| t.replace(true)))
    }
}

impl Drop for InTask {
    fn drop(&mut self) {
        IN_TASK.with(|t| t.set(self.0));
    }
}

/// Sets this thread's served request, restoring the previous one on drop
/// (return or unwind). Private and never forgotten: only
/// [`PoolRequest::serve`] and [`request_exec`] make one, each on its own
/// stack for one call.
struct Swap {
    prev: *const PoolRequest<'static>,
}

impl Swap {
    fn new(to: *const PoolRequest<'static>) -> Self {
        Self {
            prev: CURRENT.with(|c| c.replace(to)),
        }
    }
}

impl Drop for Swap {
    fn drop(&mut self) {
        CURRENT.with(|c| c.set(self.prev));
    }
}

/// The tasks of one request, claimed by index.
struct Tasks {
    /// The next unclaimed index. 64-bit so that `num_tasks` near `u32::MAX`
    /// plus one overshoot per participant cannot wrap.
    next: AtomicU64,
    num_tasks: u64,
    plugin: *const clap_plugin,
    exec: ExecFn,
}

// SAFETY: `exec` is the entry point CLAP defines for the host's pool threads
// to call, concurrently, during the plugin's `process` (`ext/thread-pool.h`);
// `plugin` is valid for that whole call, which `request_exec` is inside (the
// request is served only for the span of `process`, under the audio claim,
// and `try_run` returns after every participant has left the job). The
// counter is an atomic.
unsafe impl Sync for Tasks {}

impl Tasks {
    /// Claim and run tasks until none is left.
    fn drain(&self) {
        loop {
            // `Relaxed`: the counter only hands out distinct indices; the
            // tasks' own writes are ordered by the pool's return.
            let i = self.next.fetch_add(1, Ordering::Relaxed);
            if i >= self.num_tasks {
                return;
            }
            // SAFETY: see `impl Sync for Tasks`; `i < num_tasks <= u32::MAX`.
            unsafe { (self.exec)(self.plugin, i as u32) };
        }
    }
}

/// `clap_host_thread_pool::request_exec` for the instance whose host is
/// `host`: whether every task ran (module docs).
pub(crate) fn request_exec(host: usize, num_tasks: u32) -> bool {
    let current = CURRENT.with(Cell::get);
    if current.is_null() {
        return false;
    }
    // SAFETY: only a `Swap` sets the thread-local to non-null, from `serve`,
    // to a request that outlives the `serve` call; the `Swap` lives on
    // `serve`'s stack, is never forgotten, and restores the previous value
    // when `serve` returns or unwinds, on this same thread. So a non-null
    // value is a request still being served.
    let request = unsafe { &*current };
    if request.host != host {
        return false;
    }
    // Taken while the tasks run, so a call from inside one is refused.
    let _taken = Swap::new(ptr::null());
    request.run(num_tasks)
}
#[cfg(test)]
mod tests {
    use super::*;

    fn current() -> *const PoolRequest<'static> {
        CURRENT.with(Cell::get)
    }

    /// The served request is cleared when the span ends, by return or by
    /// unwind, so a later `request_exec` on this thread finds nothing (the
    /// integration test `a_request_after_process_returns_is_refused` covers
    /// the call; a stale pointer there would be a dangling read it cannot
    /// observe reliably).
    #[test]
    fn a_swap_restores_the_previous_request_on_return_and_unwind() {
        let fake = std::ptr::NonNull::<PoolRequest<'static>>::dangling().as_ptr()
            as *const PoolRequest<'static>;
        {
            let _s = Swap::new(fake);
            assert_eq!(current(), fake);
            {
                let _taken = Swap::new(ptr::null());
                assert!(current().is_null());
            }
            assert_eq!(current(), fake, "the inner span restores the outer");
        }
        assert!(current().is_null());
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _s = Swap::new(fake);
            panic!("unwind through the span");
        }));
        assert!(unwound.is_err());
        assert!(current().is_null(), "an unwind restores too");
    }
}
